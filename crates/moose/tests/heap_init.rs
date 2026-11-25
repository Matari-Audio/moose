//! Integration tests for `#[derive(HeapInit)]` and `HeapArray`.
//!
//! Verifies recursive in-place heap initialization (zero stack temporaries),
//! generics, `#[cfg]` attributes on fields, `Option<Box<T: HeapInit>>`,
//! `#[heap_init(with = ...)]`, `Default`-only fallback, tuple/unit structs,
//! and panic-unwind drop safety.

use moose::ProcessorSlot;
use moose::core::PluginRuntime as _;
use moose::prelude::*;
use moose_dsp::HeapArray;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default, Debug, PartialEq)]
struct DefaultOnlyField {
    tag: u32,
}

#[derive(HeapInit)]
struct DeepLeaf {
    // 256 KiB directly inside the struct (NOT boxed) — proves recursive
    // in-place initialization never materializes DeepLeaf on the stack.
    buf: [u8; 262_144],
    #[heap_init(with = 42)]
    marker: u32,
    fallback: DefaultOnlyField,
}

#[derive(HeapInit)]
struct MidLayer {
    // Two 256 KiB inline leaves (512 KiB total) + a 256 KiB HeapArray!
    left: DeepLeaf,
    right: DeepLeaf,
    table: HeapArray<f32, 65_536>,
    #[heap_init(with = 1.5)]
    gain: f64,
}

#[derive(HeapInit)]
struct GenericHolder<T, U, const N: usize> {
    direct: T,
    boxed: Box<T>,
    arr: [T; N],
    // `U` has no `HeapInit` or `Default` bound when held in `Option<Box<U>>`!
    opt_none: Option<Box<U>>,
    #[heap_init(some)]
    opt_some: Option<Box<T>>,
    #[heap_init(with = Vec::with_capacity(4))]
    items: Vec<U>,
}

struct NeitherDefaultNorHeapInit(#[allow(dead_code)] u8);

#[derive(HeapInit)]
struct CfgFieldStruct {
    always: u32,
    #[cfg(not(target_os = "none"))]
    #[heap_init(with = 77)]
    active_cfg: u32,
    #[cfg(target_os = "none")]
    inactive_cfg: NeitherDefaultNorHeapInit,
    #[heap_init(default)]
    explicit_default: DefaultOnlyField,
}

#[derive(HeapInit)]
struct TupleStruct(#[heap_init(with = 11)] u32, DeepLeaf, Option<Box<DeepLeaf>>);

#[derive(HeapInit)]
struct UnitStruct;

#[test]
fn recursive_nested_heap_init_fits_in_128k_stack() {
    assert!(std::mem::size_of::<MidLayer>() > 512 * 1024);
    let handle = std::thread::Builder::new()
        .name("derive-heap-init-128k".into())
        .stack_size(128 * 1024)
        .spawn(|| {
            let mid: Box<MidLayer> = MidLayer::heap_init();
            assert_eq!(mid.left.marker, 42);
            assert_eq!(mid.right.marker, 42);
            assert_eq!(mid.left.buf[262_143], 0);
            assert_eq!(mid.right.buf[0], 0);
            assert_eq!(mid.left.fallback, DefaultOnlyField { tag: 0 });
            assert_eq!(mid.table.len(), 65_536);
            assert!((mid.gain - 1.5).abs() < f64::EPSILON);

            let tup = TupleStruct::heap_init();
            assert_eq!(tup.0, 11);
            assert_eq!(tup.1.marker, 42);
            assert!(tup.2.is_none());

            let _unit = UnitStruct::heap_init();
        })
        .expect("spawn 128 KiB thread");
    handle.join().expect("128 KiB stack thread succeeded");
}

#[test]
fn generics_option_box_and_cfg_fields_work() {
    let holder: Box<GenericHolder<DeepLeaf, NeitherDefaultNorHeapInit, 2>> =
        GenericHolder::heap_init();
    assert_eq!(holder.direct.marker, 42);
    assert_eq!(holder.boxed.marker, 42);
    assert_eq!(holder.arr[0].marker, 42);
    assert_eq!(holder.arr[1].marker, 42);
    assert!(holder.opt_none.is_none());
    assert_eq!(holder.opt_some.as_ref().map(|b| b.marker), Some(42));
    assert_eq!(holder.items.capacity(), 4);

    let cfg_s = CfgFieldStruct::heap_init();
    assert_eq!(cfg_s.always, 0);
    #[cfg(not(target_os = "none"))]
    assert_eq!(cfg_s.active_cfg, 77);
    assert_eq!(cfg_s.explicit_default, DefaultOnlyField { tag: 0 });
}

#[test]
fn derive_heap_init_drops_earlier_fields_if_later_field_panics() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);

    struct TrackedDrop;
    impl Default for TrackedDrop {
        fn default() -> Self {
            Self
        }
    }
    impl Drop for TrackedDrop {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn panicking_init() -> u32 {
        panic!("boom during field 2 init");
    }

    #[derive(HeapInit)]
    #[allow(dead_code)]
    struct PartialPanic {
        first: TrackedDrop,
        second: TrackedDrop,
        #[heap_init(with = panicking_init())]
        third: u32,
        fourth: TrackedDrop,
    }

    DROPS.store(0, Ordering::SeqCst);
    let res = std::panic::catch_unwind(|| {
        let _ = PartialPanic::heap_init();
    });
    assert!(res.is_err());
    assert_eq!(
        DROPS.load(Ordering::SeqCst),
        2,
        "only fields initialized before the panic must drop"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn big_plugin_lifecycle_and_offthread_persist_decode_fit_in_256k_stack() {
    use moose::core::state::{DeserializedState, ParamsBackup, StateLoadQueue};
    use moose::core::wrapper::{PluginCell, apply_or_queue_state, apply_queued_state};
    use std::sync::RwLock;

    #[derive(Params)]
    #[params(heap_init)]
    struct BigParams {
        #[param(name = "Gain", range = "linear(0, 1)", default = 0.5)]
        gain: FloatParam,
        #[persist]
        seed: RwLock<u32>,
        // 128 KiB inline field inside Params (larger than any shipping host plugin's params)
        #[skip]
        _pad: [u8; 131_072],
    }

    #[derive(HeapInit)]
    struct DecodedTable {
        // 256 KiB derived DSP table built off-thread and swapped RT-safely!
        data: [u8; 262_144],
        version: u32,
    }

    #[derive(HeapInit)]
    struct BigProcessor {
        // 512 KiB directly inside Processor + 256 KiB HeapArray + 256 KiB boxed table = 1 MiB!
        scratch: [u8; 524_288],
        delay: HeapArray<f32, 65_536>,
        table: Box<DecodedTable>,
    }

    struct BigPlugin;

    impl Plugin for BigPlugin {
        type Shared = BigParams;
        type MainThread = ();
        type Processor = BigProcessor;

        fn main_thread(_shared: &Arc<BigParams>) {}

        fn activate_boxed(
            shared: &BigParams,
            _config: &AudioConfig,
            _cx: &InitContext,
        ) -> Result<Box<BigProcessor>, PreparationError> {
            let mut proc = Self::create_boxed();
            proc.table.version = *shared.seed.read().unwrap();
            Ok(proc)
        }

        fn deactivate_boxed(processor: Box<BigProcessor>) {
            drop(processor);
        }

        fn process(
            processor: &mut BigProcessor,
            shared: &BigParams,
            buffer: &mut AudioBuffer,
            _events: &EventList,
            _context: &mut ProcessContext,
        ) -> ProcessStatus {
            let g = shared.gain.value();
            let scale = if processor.table.version > 0 { g } else { 0.0 };
            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                for (o, i) in out.iter_mut().zip(inp) {
                    *o = i * scale + processor.delay[0] * 0.0 + f32::from(processor.scratch[0]);
                }
            }
            ProcessStatus::Normal
        }

        fn decode_persist_state(
            shared: &BigParams,
            _persist_bytes: &[u8],
            _context: StateContext,
        ) -> Result<Option<DecodedState>, StateLoadError> {
            let seed = *shared.seed.read().unwrap();
            let mut table = DecodedTable::heap_init();
            table.version = seed;
            table.data[262_143] = u8::try_from(seed & 0xFF).unwrap_or(0);
            Ok(Some(DecodedState::new(table)))
        }

        fn apply_decoded_persist(
            processor: &mut BigProcessor,
            _shared: &BigParams,
            decoded: &mut DecodedState,
            _context: StateContext,
        ) {
            // RT-safe pointer swap: old 256 KiB Box<DecodedTable> moves into
            // `decoded` and drops off the audio thread!
            decoded.swap_with(&mut processor.table);
        }
    }

    struct BigShell {
        params: Arc<BigParams>,
        state: Box<ProcessorSlot<BigProcessor>>,
        meters: Arc<moose::core::MeterStore>,
        snapshots: Arc<moose::core::SnapshotSlot>,
    }

    impl moose::core::plugin::PluginRuntime for BigShell {
        type Sample = f32;
        fn info() -> PluginInfo {
            PluginInfo {
                name: "BigPlugin",
                vendor: "Matari",
                url: "",
                version: "0.1.0",
                category: PluginCategory::Effect,
                description: None,
                clap_manual_url: None,
                clap_support_url: None,
                clap_features: &["audio-effect"],
                accepts_midi_in: false,
                emits_midi: false,
                midi_input_dialect: moose::core::info::MidiDialect::Midi1,
                midi_output_dialect: moose::core::info::MidiDialect::Midi1,
                midi_input_ports: 0,
                midi_output_ports: 0,
                bundle_id: "com.matari.test.big-plugin",
                vst3_id: "com.matari.test.big-plugin",
                clap_id: "com.matari.test.big-plugin",
                fourcc: *b"BigP",
                au_type: *b"aufx",
                au_manufacturer: *b"Mtri",
                vst3_subcategory: None,
                preset_user_dir: None,
                preset_extension: "bigp",
                vst3_name: None,
                clap_name: None,
                au_name: None,
                au3_name: None,
                mute_preview_output: false,
                automation: moose::core::info::Automation::DEFAULT,
                legacy_au_keys: &[],
                legacy_aax_chunks: &[],
            }
        }
        fn prepare(
            &mut self,
            config: &moose::core::plugin::PreparationConfig,
        ) -> Result<(), PreparationError> {
            <Lifecycle<BigPlugin> as moose::__reexport::moose_plugin::PluginLogicCore<f32>>::prepare(
                &mut self.state,
                &self.params,
                config,
            )
        }
        fn reset(&mut self, config: &AudioConfig) {
            <Lifecycle<BigPlugin> as moose::__reexport::moose_plugin::PluginLogicCore<f32>>::reset(
                &mut self.state,
                &self.params,
                config,
            );
        }
        fn process(
            &mut self,
            buffer: &mut AudioBuffer,
            events: &EventList,
            context: &mut ProcessContext,
        ) -> ProcessStatus {
            <Lifecycle<BigPlugin> as moose::__reexport::moose_plugin::PluginLogicCore<f32>>::process(
                &mut self.state,
                &self.params,
                buffer,
                events,
                context,
            )
        }
        fn apply_decoded_persist(&mut self, decoded: &mut DecodedState, context: StateContext) {
            <Lifecycle<BigPlugin> as moose::__reexport::moose_plugin::PluginLogicCore<f32>>::apply_decoded_persist(
                &mut self.state,
                &self.params,
                decoded,
                context,
            );
        }
    }

    impl PluginExport for BigShell {
        type Params = BigParams;
        fn create() -> Self {
            let params = <BigParams as Params>::new_arc();
            let snapshots = moose::core::SnapshotSlot::new();
            let cx = InitContext::new(None);
            let state = <Lifecycle<BigPlugin> as moose::__reexport::moose_plugin::PluginLogicCore<f32>>::init_boxed(
                &params,
                &cx,
            );
            Self {
                meters: moose::core::MeterStore::new(),
                params,
                state,
                snapshots,
            }
        }
        fn params(&self) -> &BigParams {
            &self.params
        }
        fn params_arc(&self) -> Arc<BigParams> {
            Arc::clone(&self.params)
        }
        fn meter_store(&self) -> Arc<moose::core::MeterStore> {
            Arc::clone(&self.meters)
        }
        fn snapshot_slot(&self) -> Arc<moose::core::SnapshotSlot> {
            Arc::clone(&self.snapshots)
        }
        fn decode_persist_state(
            params: &BigParams,
            persist: &[u8],
            context: StateContext,
        ) -> Result<Option<DecodedState>, StateLoadError> {
            <Lifecycle<BigPlugin> as moose::__reexport::moose_plugin::PluginLogicCore<f32>>::decode_persist_state(
                params,
                persist,
                context,
            )
        }
    }

    assert!(std::mem::size_of::<BigParams>() >= 128 * 1024);
    assert!(std::mem::size_of::<BigProcessor>() >= 512 * 1024);

    // 1. Full create -> prepare -> process -> save/load -> re-prepare -> drop
    //    fits inside a 256 KiB stack despite 128 KiB Params + 1 MiB Processor!
    moose_test::assert_lifecycle_fits::<BigShell>(256);

    // 2. Verify queued audio-thread RT swap path for `#[persist]` (`extra: None`).
    let mut shell = BigShell::create();
    shell
        .prepare(&moose::core::plugin::PreparationConfig::new(
            AudioConfig::new(48_000.0, 64),
            moose::core::plugin::PreparationFrameBound::ExplicitDriver,
        ))
        .expect("prepare");
    let params = shell.params_arc();
    let slot = shell.snapshot_slot();
    let cell = PluginCell::new(shell);
    let queue = StateLoadQueue::new(1);

    // Update persisted seed on host thread and trigger queued load (`can_own = false`, `extra = None`).
    *params.seed.write().unwrap() = 99;
    let persist_bytes = params.serialize_persist();
    let backup = ParamsBackup::capture(&*params);
    apply_or_queue_state(
        &cell,
        &*params,
        &slot,
        &queue,
        DeserializedState {
            params: vec![(0, 0.75)],
            extra: None,
            persist: persist_bytes,
            ..Default::default()
        },
        &backup,
        false,
    )
    .expect("queued persist decode succeeds");

    // Audio thread pops and swaps the pre-decoded 256 KiB table in place.
    {
        let mut guard = cell.enter();
        assert!(matches!(
            apply_queued_state(&mut *guard, &queue),
            Some(Ok(()))
        ));
        let proc = guard.state.processor().expect("active");
        assert_eq!(proc.table.version, 99);
        assert_eq!(proc.table.data[262_143], 99);
    }
    // Host reclaims the retired 256 KiB old table off the audio thread.
    queue.reclaim_retired();
}
