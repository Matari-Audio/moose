//! Cross-format behavioural contract (Wave 3C).
//!
//! One plugin, [`fixture::Contract`], driven through three real C ABIs in
//! this process:
//!
//! * **CLAP**: `moose_clap::create_plugin_instance` plus a real `clap_host`
//!   with params / latency / tail / state host extensions, then only the
//!   `clap_plugin` vtable and the extensions it hands out.
//! * **VST3**: `moose_vst3::register_vst3` + `moose_vst3::ffi::moose_vst3_get_factory`,
//!   then `IPluginFactory::createInstance`, and only COM calls from there on
//!   (`IComponent`, `IAudioProcessor`, `IEditController`, host-side
//!   `IBStream` / `IEventList` / `IParameterChanges`).
//! * **AU**: `moose_au::callbacks`, the callback table the AU v2 / v3 shims
//!   call, runnable without Apple frameworks.
//!
//! Each scenario is written once against the format-neutral [`Harness`]
//! trait and run for every format, so a divergence between wrappers fails
//! as a single assertion that names the format. Divergences this suite
//! found are kept as `#[ignore]`d regression tests in [`divergences`], each
//! naming the wrapper track that owns the fix.

#![allow(
    clippy::float_cmp,
    reason = "contract values are exact plain values, compared exactly on purpose"
)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "host ABIs use mixed integer widths; every value here is small"
)]
#![allow(clippy::too_many_lines, reason = "scenario bodies read top to bottom")]
#![allow(
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks,
    reason = "test harness FFI; every pointer is owned by the harness for the call"
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

// ---------------------------------------------------------------------------
// Allocation counter: counts heap operations on the current thread while a
// measured region is open. Global to this test binary only.
// ---------------------------------------------------------------------------

struct CountingAlloc;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static HEAP_OPS: Cell<usize> = const { Cell::new(0) };
}

fn note_heap_op() {
    // `try_with`: allocations during thread teardown must not panic.
    let _ = COUNTING.try_with(|on| {
        if on.get() {
            let _ = HEAP_OPS.try_with(|n| n.set(n.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_heap_op();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note_heap_op();
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_heap_op();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_heap_op();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Heap operations `f` performed on this thread.
fn heap_ops<R>(f: impl FnOnce() -> R) -> (R, usize) {
    HEAP_OPS.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    let r = f();
    COUNTING.with(|c| c.set(false));
    (r, HEAP_OPS.with(Cell::get))
}

// ---------------------------------------------------------------------------
// The shared fixture plugin
// ---------------------------------------------------------------------------

mod fixture {
    use moose::core::bus::BusLayout;
    use moose::core::state::{DeserializedState, StateLoadError, StateMigrationChain};
    use moose::prelude::*;
    use moose_core::buffer::AudioBuffer;
    use moose_core::config::{AudioConfig, ProcessMode};
    use moose_core::events::{EventBody, EventList};
    use moose_core::export::PluginExport;
    use moose_core::info::{Automation, MidiDialect, PluginCategory, PluginInfo};
    use moose_core::meters::MeterStore;
    use moose_core::plugin::PluginRuntime;
    use moose_core::process::{ProcessContext, ProcessStatus};
    use moose_core::snapshot::SnapshotSlot;
    use std::cell::{Cell, RefCell};
    use std::sync::{RwLock, Weak};

    pub const CLAP_ID: &str = "org.moose.contract";
    pub const LATENCY: u32 = 64;
    pub const TAIL: u32 = 4800;
    /// Renumbered IDs: `mix` now owns numeric ID 10, which `cutoff` used to
    /// have before it moved to 100.
    pub const MIX: u32 = 10;
    pub const CUTOFF: u32 = 100;
    pub const GAIN: u32 = 200;
    pub const BANK: u32 = 300;
    pub const INITIAL_LABELS: [&str; 3] = ["Init", "Warm", "Bright"];
    /// `load_state` rejects this custom-state blob (after params applied).
    pub const POISON_EXTRA: &[u8] = b"poison";
    /// `validate_state` rejects this one up front.
    pub const INVALID_EXTRA: &[u8] = b"invalid";

    #[derive(Params)]
    pub struct ContractParams {
        #[param(
            id = 10,
            key = "mix",
            name = "Mix",
            range = "linear(0, 1)",
            default = 0.25
        )]
        pub mix: FloatParam,
        #[param(
            id = 100,
            key = "cutoff",
            name = "Cutoff",
            range = "linear(20, 20000)",
            default = 1000.0
        )]
        pub cutoff: FloatParam,
        #[param(
            id = 200,
            key = "gain",
            name = "Gain",
            range = "linear(0, 1)",
            default = 0.5
        )]
        pub gain: FloatParam,
        #[param(id = 300, key = "bank", name = "Bank")]
        pub bank: ValueListParam,
        #[persist]
        pub label: RwLock<String>,
    }

    impl ContractParams {
        pub fn label(&self) -> String {
            self.label.read().unwrap().clone()
        }
        pub fn set_label(&self, label: &str) {
            label.clone_into(&mut self.label.write().unwrap());
        }
        /// The `#[persist]` blob a store with `label` would save.
        pub fn persist_blob(label: &str) -> Vec<u8> {
            let p = Self::new();
            p.set_label(label);
            p.serialize_persist()
        }
    }

    /// What the plugin saw in its `process` event list.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Seen {
        NoteOn { at: u32, key: u8 },
        NoteOff { at: u32, key: u8 },
        Param { at: u32, id: u32 },
    }

    thread_local! {
        /// Every test drives its instances on its own thread, so these
        /// per-thread probes never mix tests that run in parallel.
        pub static SEEN: RefCell<Vec<Seen>> = RefCell::new(Vec::with_capacity(4096));
        pub static PROCESS_CALLS: Cell<usize> = const { Cell::new(0) };
        /// Host-block position of the current sub-block: wrappers split a
        /// host block at automation points, and each sub-block's event
        /// offsets are relative to its own start.
        pub static BLOCK_POS: Cell<u32> = const { Cell::new(0) };
        pub static RESETS: Cell<usize> = const { Cell::new(0) };
        pub static LAST_RATE: Cell<f64> = const { Cell::new(0.0) };
        pub static LAST_MODE: Cell<Option<ProcessMode>> = const { Cell::new(None) };
        pub static LAST_PARAMS: RefCell<Weak<ContractParams>> = const { RefCell::new(Weak::new()) };
        pub static MIGRATIONS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
        pub static LOADED_EXTRA: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    /// The params of the newest instance created on this thread.
    pub fn live() -> Arc<ContractParams> {
        LAST_PARAMS
            .with(|p| p.borrow().upgrade())
            .expect("an instance is alive on this thread")
    }

    /// Drain what the plugin saw and start a new host block.
    pub fn take_seen() -> Vec<Seen> {
        BLOCK_POS.with(|p| p.set(0));
        SEEN.with(|s| s.borrow_mut().drain(..).collect())
    }

    pub fn take_migrations() -> Vec<u32> {
        MIGRATIONS.with(|m| std::mem::take(&mut *m.borrow_mut()))
    }

    fn append_label(state: &mut DeserializedState, suffix: &str) {
        let p = ContractParams::new();
        p.load_persist(&state.persist);
        let label = format!("{}|{suffix}", p.label());
        state.persist = ContractParams::persist_blob(&label);
    }

    /// Schema 0 -> 1: v1 envelopes stored gain at half scale.
    #[allow(clippy::unnecessary_wraps)]
    fn m0(state: &mut DeserializedState) -> Result<(), StateLoadError> {
        MIGRATIONS.with(|m| m.borrow_mut().push(0));
        for (id, value) in &mut state.params {
            if *id == GAIN {
                *value = (*value * 2.0).min(1.0);
            }
        }
        append_label(state, "m0");
        Ok(())
    }

    /// Schema 1 -> 2: params untouched, persist stamped.
    #[allow(clippy::unnecessary_wraps)]
    fn m1(state: &mut DeserializedState) -> Result<(), StateLoadError> {
        MIGRATIONS.with(|m| m.borrow_mut().push(1));
        append_label(state, "m1");
        Ok(())
    }

    pub struct Contract {
        params: Arc<ContractParams>,
        meters: Arc<MeterStore>,
        snapshot: Arc<SnapshotSlot>,
        extra: Vec<u8>,
    }

    impl PluginExport for Contract {
        type Params = ContractParams;
        fn create() -> Self {
            let params = Arc::new(ContractParams::new());
            params.bank.set_labels(INITIAL_LABELS);
            let _ = params.take_catalog_changes();
            LAST_PARAMS.with(|p| *p.borrow_mut() = Arc::downgrade(&params));
            Self {
                params,
                meters: MeterStore::new(),
                snapshot: SnapshotSlot::new(),
                extra: b"contract-extra".to_vec(),
            }
        }
        fn params(&self) -> &ContractParams {
            &self.params
        }
        fn params_arc(&self) -> Arc<ContractParams> {
            Arc::clone(&self.params)
        }
        fn meter_store(&self) -> Arc<MeterStore> {
            Arc::clone(&self.meters)
        }
        fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
            Arc::clone(&self.snapshot)
        }
    }

    impl PluginRuntime for Contract {
        type Sample = f32;
        fn info() -> PluginInfo {
            PluginInfo {
                name: "Contract",
                vendor: "moose",
                url: "https://example.com",
                version: "1.0.0",
                category: PluginCategory::Effect,
                description: None,
                clap_manual_url: None,
                clap_support_url: None,
                clap_features: &[],
                accepts_midi_in: true,
                emits_midi: false,
                midi_input_dialect: MidiDialect::Midi1,
                midi_output_dialect: MidiDialect::Midi1,
                midi_input_ports: 1,
                midi_output_ports: 0,
                bundle_id: "contract",
                vst3_id: CLAP_ID,
                clap_id: CLAP_ID,
                fourcc: *b"Cntr",
                au_type: *b"aumf",
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
            }
        }
        fn bus_layouts() -> Vec<BusLayout> {
            vec![BusLayout::stereo()]
        }
        fn reset(&mut self, config: &AudioConfig) {
            RESETS.with(|r| r.set(r.get() + 1));
            LAST_RATE.with(|r| r.set(config.sample_rate));
        }
        fn latency(&self) -> u32 {
            LATENCY
        }
        fn tail(&self) -> u32 {
            TAIL
        }
        fn save_state(&self) -> Vec<u8> {
            self.extra.clone()
        }
        fn load_state(&mut self, data: &[u8]) -> Result<(), StateLoadError> {
            if data == POISON_EXTRA {
                return Err(StateLoadError::Other("poisoned custom state".into()));
            }
            data.clone_into(&mut self.extra);
            // AU saves from the published slot even while idle. Keep the
            // fixture's snapshot consistent with its plugin-owned payload.
            self.snapshot
                .store_inline(|buf| buf.extend_from_slice(data));
            LOADED_EXTRA.with(|e| data.clone_into(&mut e.borrow_mut()));
            Ok(())
        }
        fn validate_state(data: &[u8]) -> Result<(), StateLoadError> {
            if data == INVALID_EXTRA {
                return Err(StateLoadError::Other("invalid custom state".into()));
            }
            Ok(())
        }
        fn state_schema_version() -> u32 {
            2
        }
        fn state_migrations() -> StateMigrationChain {
            StateMigrationChain::new(2).step(0, m0).step(1, m1)
        }
        fn process(
            &mut self,
            audio: &mut AudioBuffer,
            events: &EventList,
            ctx: &mut ProcessContext,
        ) -> ProcessStatus {
            PROCESS_CALLS.with(|n| n.set(n.get() + 1));
            LAST_MODE.with(|m| m.set(Some(ctx.process_mode)));
            let base = BLOCK_POS.with(Cell::get);
            BLOCK_POS.with(|p| p.set(base + audio.num_samples() as u32));
            SEEN.with(|seen| {
                let mut seen = seen.borrow_mut();
                for ev in events.iter() {
                    let at = base + ev.sample_offset;
                    let entry = match ev.body {
                        EventBody::NoteOn { note, .. } => Seen::NoteOn { at, key: note },
                        EventBody::NoteOff { note, .. } => Seen::NoteOff { at, key: note },
                        EventBody::ParamChange { id, .. } | EventBody::ParamRamp { id, .. } => {
                            Seen::Param { at, id }
                        }
                        _ => continue,
                    };
                    // Never grow on the audio thread.
                    if seen.len() < seen.capacity() {
                        seen.push(entry);
                    }
                }
            });
            let gain = self.params.gain.raw_target() as f32;
            for ch in 0..audio.channels() {
                let (input, output) = audio.io(ch);
                for (o, i) in output.iter_mut().zip(input) {
                    *o = *i * gain;
                }
            }
            ProcessStatus::Normal
        }
    }
}

use fixture::{BANK, CUTOFF, Contract, ContractParams, GAIN, MIX, Seen};
use moose_core::config::ProcessMode;
use moose_core::state::{hash_plugin_id, serialize_state, serialize_state_v2};
use moose_params::Params as _;

// ---------------------------------------------------------------------------
// Format-neutral harness
// ---------------------------------------------------------------------------

/// One host event for a block, in host (time) order.
#[derive(Clone, Copy, Debug)]
enum HostEvent {
    NoteOn {
        at: u32,
        key: u8,
    },
    NoteOff {
        at: u32,
        key: u8,
    },
    /// Plain value; each harness converts to its wire representation.
    Param {
        at: u32,
        id: u32,
        plain: f64,
    },
}

const MAX_FRAMES: u32 = 512;

trait Harness {
    const NAME: &'static str;
    fn create() -> Self;
    /// Prepare at `rate` and start processing.
    fn start(&mut self, rate: f64);
    /// Stop processing and release.
    fn stop(&mut self);
    /// Host-side reset while prepared (CLAP `reset`, VST3 off/on processing,
    /// AU `AudioUnitReset` at the same config).
    fn reset(&mut self);
    /// Render `frames` of a stereo block whose input is `input` on both
    /// channels. Returns the two output channels.
    fn process(&mut self, frames: u32, input: f32, events: &[HostEvent]) -> [Vec<f32>; 2];
    /// Deliver parameter values while the instance is suspended (no audio).
    fn suspended_param_flush(&mut self, events: &[(u32, f64)]);
    fn get_plain(&self, id: u32) -> f64;
    fn format(&self, id: u32, plain: f64) -> Option<String>;
    fn parse(&self, id: u32, text: &str) -> Option<f64>;
    fn save(&self) -> Vec<u8>;
    fn load(&mut self, blob: &[u8]) -> bool;
    fn latency(&self) -> u32;
    fn tail(&self) -> u32;
    fn set_offline(&mut self, offline: bool);
}

fn hash() -> u64 {
    hash_plugin_id(fixture::CLAP_ID)
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

// ---------------------------------------------------------------------------
// CLAP host harness
// ---------------------------------------------------------------------------

mod clap_host_harness {
    use super::{Contract, HostEvent, MAX_FRAMES};
    use clap_sys::audio_buffer::clap_audio_buffer;
    use clap_sys::events::{
        CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_VALUE,
        clap_event_header, clap_event_note, clap_event_param_value, clap_input_events,
        clap_output_events,
    };
    use clap_sys::ext::audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_plugin_audio_ports};
    use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_host_latency, clap_plugin_latency};
    use clap_sys::ext::note_ports::{CLAP_EXT_NOTE_PORTS, clap_plugin_note_ports};
    use clap_sys::ext::params::{
        CLAP_EXT_PARAMS, clap_host_params, clap_param_clear_flags, clap_param_info,
        clap_param_rescan_flags, clap_plugin_params,
    };
    use clap_sys::ext::render::{
        CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE, CLAP_RENDER_REALTIME, clap_plugin_render,
    };
    use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state, clap_plugin_state};
    use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_host_tail, clap_plugin_tail};
    use clap_sys::host::clap_host;
    use clap_sys::id::clap_id;
    use clap_sys::plugin::clap_plugin;
    use clap_sys::process::clap_process;
    use clap_sys::stream::{clap_istream, clap_ostream};
    use moose_clap::DescriptorHolder;
    use moose_core::plugin::PluginRuntime;
    use std::cell::{Cell, RefCell};
    use std::ffi::{CStr, CString, c_char, c_void};
    use std::ptr;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// What the plugin asked of the host. `host_data` points here.
    #[derive(Default)]
    pub struct HostLog {
        pub rescans: AtomicU32,
        pub rescan_flags: AtomicU32,
        pub clears: AtomicU32,
        pub flush_requests: AtomicU32,
        pub callbacks: AtomicU32,
        pub restarts: AtomicU32,
        pub process_requests: AtomicU32,
        pub latency_changed: AtomicU32,
        pub tail_changed: AtomicU32,
        pub dirty: AtomicU32,
    }

    unsafe fn log<'a>(host: *const clap_host) -> &'a HostLog {
        unsafe { &*(*host).host_data.cast::<HostLog>() }
    }

    unsafe extern "C" fn rescan(host: *const clap_host, flags: clap_param_rescan_flags) {
        let log = unsafe { log(host) };
        log.rescans.fetch_add(1, Ordering::Relaxed);
        log.rescan_flags.fetch_or(flags, Ordering::Relaxed);
    }
    unsafe extern "C" fn clear(host: *const clap_host, _: clap_id, _: clap_param_clear_flags) {
        unsafe { log(host) }.clears.fetch_add(1, Ordering::Relaxed);
    }
    unsafe extern "C" fn request_flush(host: *const clap_host) {
        unsafe { log(host) }
            .flush_requests
            .fetch_add(1, Ordering::Relaxed);
    }
    unsafe extern "C" fn latency_changed(host: *const clap_host) {
        unsafe { log(host) }
            .latency_changed
            .fetch_add(1, Ordering::Relaxed);
    }
    unsafe extern "C" fn tail_changed(host: *const clap_host) {
        unsafe { log(host) }
            .tail_changed
            .fetch_add(1, Ordering::Relaxed);
    }
    unsafe extern "C" fn mark_dirty(host: *const clap_host) {
        unsafe { log(host) }.dirty.fetch_add(1, Ordering::Relaxed);
    }
    unsafe extern "C" fn request_restart(host: *const clap_host) {
        unsafe { log(host) }
            .restarts
            .fetch_add(1, Ordering::Relaxed);
    }
    unsafe extern "C" fn request_process(host: *const clap_host) {
        unsafe { log(host) }
            .process_requests
            .fetch_add(1, Ordering::Relaxed);
    }
    unsafe extern "C" fn request_callback(host: *const clap_host) {
        unsafe { log(host) }
            .callbacks
            .fetch_add(1, Ordering::Relaxed);
    }

    static HOST_PARAMS: clap_host_params = clap_host_params {
        rescan: Some(rescan),
        clear: Some(clear),
        request_flush: Some(request_flush),
    };
    static HOST_LATENCY: clap_host_latency = clap_host_latency {
        changed: Some(latency_changed),
    };
    static HOST_TAIL: clap_host_tail = clap_host_tail {
        changed: Some(tail_changed),
    };
    static HOST_STATE: clap_host_state = clap_host_state {
        mark_dirty: Some(mark_dirty),
    };

    unsafe extern "C" fn get_extension(_: *const clap_host, id: *const c_char) -> *const c_void {
        let id = unsafe { CStr::from_ptr(id) };
        if id == CLAP_EXT_PARAMS {
            (&raw const HOST_PARAMS).cast()
        } else if id == CLAP_EXT_LATENCY {
            (&raw const HOST_LATENCY).cast()
        } else if id == CLAP_EXT_TAIL {
            (&raw const HOST_TAIL).cast()
        } else if id == CLAP_EXT_STATE {
            (&raw const HOST_STATE).cast()
        } else {
            ptr::null()
        }
    }

    /// Input event storage with a stable address for the block.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub union ClapEvent {
        header: clap_event_header,
        note: clap_event_note,
        param: clap_event_param_value,
    }

    impl ClapEvent {
        fn header(size: usize, time: u32, kind: u16) -> clap_event_header {
            clap_event_header {
                size: size as u32,
                time,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: kind,
                flags: 0,
            }
        }
        pub fn note(at: u32, key: u8, on: bool) -> Self {
            Self {
                note: clap_event_note {
                    header: Self::header(
                        size_of::<clap_event_note>(),
                        at,
                        if on {
                            CLAP_EVENT_NOTE_ON
                        } else {
                            CLAP_EVENT_NOTE_OFF
                        },
                    ),
                    note_id: -1,
                    port_index: 0,
                    channel: 0,
                    key: i16::from(key),
                    velocity: if on { 1.0 } else { 0.0 },
                },
            }
        }
        pub fn param(at: u32, id: u32, plain: f64, cookie: *mut c_void) -> Self {
            Self {
                param: clap_event_param_value {
                    header: Self::header(
                        size_of::<clap_event_param_value>(),
                        at,
                        CLAP_EVENT_PARAM_VALUE,
                    ),
                    param_id: id,
                    cookie,
                    note_id: -1,
                    port_index: -1,
                    channel: -1,
                    key: -1,
                    value: plain,
                },
            }
        }
        pub fn from_host(ev: &HostEvent) -> Self {
            match *ev {
                HostEvent::NoteOn { at, key } => Self::note(at, key, true),
                HostEvent::NoteOff { at, key } => Self::note(at, key, false),
                HostEvent::Param { at, id, plain } => Self::param(at, id, plain, ptr::null_mut()),
            }
        }
    }

    unsafe extern "C" fn in_size(list: *const clap_input_events) -> u32 {
        unsafe { (*(*list).ctx.cast::<Vec<ClapEvent>>()).len() as u32 }
    }
    unsafe extern "C" fn in_get(
        list: *const clap_input_events,
        i: u32,
    ) -> *const clap_event_header {
        let events = unsafe { &*(*list).ctx.cast::<Vec<ClapEvent>>() };
        events
            .get(i as usize)
            .map_or(ptr::null(), |e| &raw const e.header)
    }
    unsafe extern "C" fn out_push(
        _: *const clap_output_events,
        _: *const clap_event_header,
    ) -> bool {
        true
    }

    pub fn input_events(events: &Vec<ClapEvent>) -> clap_input_events {
        clap_input_events {
            ctx: ptr::from_ref(events).cast_mut().cast(),
            size: Some(in_size),
            get: Some(in_get),
        }
    }

    pub fn output_events() -> clap_output_events {
        clap_output_events {
            ctx: ptr::null_mut(),
            try_push: Some(out_push),
        }
    }

    struct ReadCursor {
        data: Vec<u8>,
        pos: Cell<usize>,
    }
    unsafe extern "C" fn stream_read(s: *const clap_istream, buf: *mut c_void, size: u64) -> i64 {
        let cursor = unsafe { &*(*s).ctx.cast::<ReadCursor>() };
        let pos = cursor.pos.get();
        let n = (cursor.data.len() - pos).min(size as usize);
        unsafe { ptr::copy_nonoverlapping(cursor.data.as_ptr().add(pos), buf.cast(), n) };
        cursor.pos.set(pos + n);
        n as i64
    }
    unsafe extern "C" fn stream_write(
        s: *const clap_ostream,
        buf: *const c_void,
        size: u64,
    ) -> i64 {
        let out = unsafe { &*(*s).ctx.cast::<RefCell<Vec<u8>>>() };
        out.borrow_mut().extend_from_slice(unsafe {
            std::slice::from_raw_parts(buf.cast::<u8>(), size as usize)
        });
        size as i64
    }

    /// A real `clap_host` plus one plugin instance created through it.
    pub struct Clap {
        pub log: std::sync::Arc<HostLog>,
        host: *mut clap_host,
        descriptor: *mut DescriptorHolder,
        _strings: [CString; 4],
        pub plugin: *const clap_plugin,
        pub active: bool,
        pub processing: bool,
        pub rate: f64,
    }

    impl Clap {
        pub fn new() -> Self {
            let log = std::sync::Arc::<HostLog>::default();
            let strings = [
                CString::new("contract-host").unwrap(),
                CString::new("moose").unwrap(),
                CString::new("https://example.com").unwrap(),
                CString::new("1.0").unwrap(),
            ];
            let host = Box::into_raw(Box::new(clap_host {
                clap_version: clap_sys::version::CLAP_VERSION,
                host_data: std::sync::Arc::as_ptr(&log).cast_mut().cast(),
                name: strings[0].as_ptr(),
                vendor: strings[1].as_ptr(),
                url: strings[2].as_ptr(),
                version: strings[3].as_ptr(),
                get_extension: Some(get_extension),
                request_restart: Some(request_restart),
                request_process: Some(request_process),
                request_callback: Some(request_callback),
            }));
            let descriptor = Box::into_raw(Box::new(DescriptorHolder::new(&Contract::info())));
            let plugin = unsafe {
                moose_clap::create_plugin_instance::<Contract>(
                    &raw const (*descriptor).descriptor,
                    host,
                )
            };
            assert!(!plugin.is_null(), "CLAP create_plugin_instance");
            let owner = Self {
                log,
                host,
                descriptor,
                _strings: strings,
                plugin,
                active: false,
                processing: false,
                rate: 0.0,
            };
            assert!(unsafe { (*plugin).init.unwrap()(plugin) }, "CLAP init");
            owner
        }

        pub fn ext<T>(&self, id: &CStr) -> Option<&T> {
            let p = unsafe { (*self.plugin).get_extension.unwrap()(self.plugin, id.as_ptr()) };
            unsafe { p.cast::<T>().as_ref() }
        }
        pub fn params(&self) -> &clap_plugin_params {
            self.ext(CLAP_EXT_PARAMS).expect("clap.params")
        }
        pub fn state(&self) -> &clap_plugin_state {
            self.ext(CLAP_EXT_STATE).expect("clap.state")
        }
        pub fn latency_ext(&self) -> &clap_plugin_latency {
            self.ext(CLAP_EXT_LATENCY).expect("clap.latency")
        }
        pub fn tail_ext(&self) -> &clap_plugin_tail {
            self.ext(CLAP_EXT_TAIL).expect("clap.tail")
        }
        pub fn audio_ports(&self) -> &clap_plugin_audio_ports {
            self.ext(CLAP_EXT_AUDIO_PORTS).expect("clap.audio-ports")
        }
        pub fn note_ports(&self) -> &clap_plugin_note_ports {
            self.ext(CLAP_EXT_NOTE_PORTS).expect("clap.note-ports")
        }
        pub fn render(&self) -> &clap_plugin_render {
            self.ext(CLAP_EXT_RENDER).expect("clap.render")
        }

        pub fn activate(&mut self, rate: f64) -> bool {
            let ok = unsafe { (*self.plugin).activate.unwrap()(self.plugin, rate, 1, MAX_FRAMES) };
            self.active = ok;
            self.rate = rate;
            ok
        }
        pub fn deactivate(&mut self) {
            unsafe { (*self.plugin).deactivate.unwrap()(self.plugin) };
            self.active = false;
        }
        pub fn start_processing(&mut self) -> bool {
            let ok = unsafe { (*self.plugin).start_processing.unwrap()(self.plugin) };
            self.processing = ok;
            ok
        }
        pub fn stop_processing(&mut self) {
            unsafe { (*self.plugin).stop_processing.unwrap()(self.plugin) };
            self.processing = false;
        }
        pub fn on_main_thread(&self) {
            unsafe { (*self.plugin).on_main_thread.unwrap()(self.plugin) };
        }

        pub fn param_info(&self, index: u32) -> Option<clap_param_info> {
            let mut info: clap_param_info = unsafe { std::mem::zeroed() };
            unsafe { (self.params().get_info.unwrap())(self.plugin, index, &raw mut info) }
                .then_some(info)
        }

        pub fn info_by_id(&self, id: u32) -> Option<clap_param_info> {
            let count = unsafe { (self.params().count.unwrap())(self.plugin) };
            (0..count)
                .filter_map(|i| self.param_info(i))
                .find(|info| info.id == id)
        }

        pub fn flush(&self, events: &Vec<ClapEvent>) {
            let input = input_events(events);
            let output = output_events();
            unsafe {
                (self.params().flush.unwrap())(self.plugin, &raw const input, &raw const output);
            }
        }

        /// One process call with pre-built events; returns the CLAP status.
        pub fn process_raw(
            &self,
            frames: u32,
            inputs: &mut [Vec<f32>; 2],
            outputs: &mut [Vec<f32>; 2],
            events: &Vec<ClapEvent>,
        ) -> i32 {
            let mut in_ptrs = [inputs[0].as_mut_ptr(), inputs[1].as_mut_ptr()];
            let mut out_ptrs = [outputs[0].as_mut_ptr(), outputs[1].as_mut_ptr()];
            let audio_in = clap_audio_buffer {
                data32: in_ptrs.as_mut_ptr(),
                data64: ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let mut audio_out = clap_audio_buffer {
                data32: out_ptrs.as_mut_ptr(),
                data64: ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let input = input_events(events);
            let output = output_events();
            let process = clap_process {
                steady_time: -1,
                frames_count: frames,
                transport: ptr::null(),
                audio_inputs: &raw const audio_in,
                audio_outputs: &raw mut audio_out,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: &raw const input,
                out_events: &raw const output,
            };
            unsafe { (*self.plugin).process.unwrap()(self.plugin, &raw const process) }
        }

        pub fn save_state(&self) -> Option<Vec<u8>> {
            let out = RefCell::new(Vec::new());
            let stream = clap_ostream {
                ctx: ptr::from_ref(&out).cast_mut().cast(),
                write: Some(stream_write),
            };
            unsafe { (self.state().save.unwrap())(self.plugin, &raw const stream) }
                .then(|| out.into_inner())
        }

        pub fn load_state(&self, blob: &[u8]) -> bool {
            let cursor = ReadCursor {
                data: blob.to_vec(),
                pos: Cell::new(0),
            };
            let stream = clap_istream {
                ctx: ptr::from_ref(&cursor).cast_mut().cast(),
                read: Some(stream_read),
            };
            unsafe { (self.state().load.unwrap())(self.plugin, &raw const stream) }
        }

        pub fn set_render(&self, offline: bool) -> bool {
            let mode = if offline {
                CLAP_RENDER_OFFLINE
            } else {
                CLAP_RENDER_REALTIME
            };
            unsafe { (self.render().set.unwrap())(self.plugin, mode) }
        }
    }

    impl Drop for Clap {
        fn drop(&mut self) {
            if self.processing {
                self.stop_processing();
            }
            if self.active {
                self.deactivate();
            }
            unsafe { (*self.plugin).destroy.unwrap()(self.plugin) };
            // Reclaim ABI allocations only after the plugin stops using them.
            unsafe {
                drop(Box::from_raw(self.host));
                drop(Box::from_raw(self.descriptor));
            }
        }
    }
}

impl Harness for clap_host_harness::Clap {
    const NAME: &'static str = "CLAP";
    fn create() -> Self {
        Self::new()
    }
    fn start(&mut self, rate: f64) {
        assert!(self.activate(rate), "CLAP activate");
        assert!(self.start_processing(), "CLAP start_processing");
    }
    fn stop(&mut self) {
        self.stop_processing();
        self.deactivate();
    }
    fn reset(&mut self) {
        unsafe { (*self.plugin).reset.unwrap()(self.plugin) };
    }
    fn process(&mut self, frames: u32, input: f32, events: &[HostEvent]) -> [Vec<f32>; 2] {
        let events: Vec<_> = events
            .iter()
            .map(clap_host_harness::ClapEvent::from_host)
            .collect();
        let mut inputs = [vec![input; frames as usize], vec![input; frames as usize]];
        let mut outputs = [
            vec![f32::NAN; frames as usize],
            vec![f32::NAN; frames as usize],
        ];
        let status = self.process_raw(frames, &mut inputs, &mut outputs, &events);
        assert!(status >= 1, "CLAP process status {status}");
        outputs
    }
    fn suspended_param_flush(&mut self, events: &[(u32, f64)]) {
        let events: Vec<_> = events
            .iter()
            .map(|&(id, plain)| {
                clap_host_harness::ClapEvent::param(0, id, plain, std::ptr::null_mut())
            })
            .collect();
        self.flush(&events);
    }
    fn get_plain(&self, id: u32) -> f64 {
        let mut v = f64::NAN;
        assert!(unsafe { (self.params().get_value.unwrap())(self.plugin, id, &raw mut v) });
        v
    }
    fn format(&self, id: u32, plain: f64) -> Option<String> {
        let mut buf = [0 as std::ffi::c_char; 256];
        unsafe {
            (self.params().value_to_text.unwrap())(self.plugin, id, plain, buf.as_mut_ptr(), 256)
        }
        .then(|| {
            unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
    }
    fn parse(&self, id: u32, text: &str) -> Option<f64> {
        let text = std::ffi::CString::new(text).unwrap();
        let mut v = f64::NAN;
        unsafe {
            (self.params().text_to_value.unwrap())(self.plugin, id, text.as_ptr(), &raw mut v)
        }
        .then_some(v)
    }
    fn save(&self) -> Vec<u8> {
        self.save_state().expect("CLAP state save")
    }
    fn load(&mut self, blob: &[u8]) -> bool {
        self.load_state(blob)
    }
    fn latency(&self) -> u32 {
        unsafe { (self.latency_ext().get.unwrap())(self.plugin) }
    }
    fn tail(&self) -> u32 {
        unsafe { (self.tail_ext().get.unwrap())(self.plugin) }
    }
    fn set_offline(&mut self, offline: bool) {
        assert!(self.set_render(offline));
    }
}

// ---------------------------------------------------------------------------
// VST3 host harness (COM only after the factory)
// ---------------------------------------------------------------------------

mod vst3_host_harness {
    use super::{Contract, HostEvent, MAX_FRAMES};
    use std::ffi::c_void;
    use std::sync::{Mutex, Once};
    use vst3::Steinberg::Vst::*;
    use vst3::Steinberg::*;
    use vst3::{Class, ComPtr, ComWrapper, Interface};

    pub fn ensure_registered() {
        static ONCE: Once = Once::new();
        ONCE.call_once(moose_vst3::register_vst3::<Contract>);
    }

    // -- host-side COM objects -------------------------------------------

    /// Memory `IBStream`.
    pub struct MemStream {
        pub data: Mutex<(Vec<u8>, usize)>,
    }
    impl MemStream {
        pub fn new(bytes: Vec<u8>) -> ComWrapper<Self> {
            ComWrapper::new(Self {
                data: Mutex::new((bytes, 0)),
            })
        }
    }
    impl Class for MemStream {
        type Interfaces = (IBStream,);
    }
    impl IBStreamTrait for MemStream {
        unsafe fn read(&self, buffer: *mut c_void, n: int32, read: *mut int32) -> tresult {
            let mut guard = self.data.lock().unwrap();
            let (data, pos) = &mut *guard;
            let count = (data.len() - *pos).min(n.max(0) as usize);
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().add(*pos), buffer.cast(), count) };
            *pos += count;
            if !read.is_null() {
                unsafe { *read = count as i32 };
            }
            kResultOk
        }
        unsafe fn write(&self, buffer: *mut c_void, n: int32, written: *mut int32) -> tresult {
            let mut guard = self.data.lock().unwrap();
            let (data, pos) = &mut *guard;
            let bytes =
                unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n.max(0) as usize) };
            data.truncate(*pos);
            data.extend_from_slice(bytes);
            *pos = data.len();
            if !written.is_null() {
                unsafe { *written = n };
            }
            kResultOk
        }
        unsafe fn seek(&self, offset: int64, mode: int32, result: *mut int64) -> tresult {
            let mut guard = self.data.lock().unwrap();
            let (data, pos) = &mut *guard;
            let base = match mode {
                0 => 0,
                1 => *pos as i64,
                2 => data.len() as i64,
                _ => return kInvalidArgument,
            };
            *pos = (base + offset).clamp(0, data.len() as i64) as usize;
            if !result.is_null() {
                unsafe { *result = *pos as i64 };
            }
            kResultOk
        }
        unsafe fn tell(&self, result: *mut int64) -> tresult {
            if !result.is_null() {
                unsafe { *result = self.data.lock().unwrap().1 as i64 };
            }
            kResultOk
        }
    }

    pub struct Events(pub Vec<Event>);
    impl Class for Events {
        type Interfaces = (IEventList,);
    }
    impl IEventListTrait for Events {
        unsafe fn getEventCount(&self) -> i32 {
            self.0.len() as i32
        }
        unsafe fn getEvent(&self, index: i32, out: *mut Event) -> tresult {
            match self.0.get(index as usize) {
                Some(event) => {
                    unsafe { out.write(*event) };
                    kResultOk
                }
                None => kResultFalse,
            }
        }
        unsafe fn addEvent(&self, _: *mut Event) -> tresult {
            kResultOk
        }
    }

    pub fn note(at: u32, key: u8, on: bool) -> Event {
        let mut event: Event = unsafe { std::mem::zeroed() };
        event.sampleOffset = at as i32;
        if on {
            event.r#type = Event_::EventTypes_::kNoteOnEvent as u16;
            event.__field0.noteOn = NoteOnEvent {
                channel: 0,
                pitch: i16::from(key),
                tuning: 0.0,
                velocity: 1.0,
                length: 0,
                noteId: -1,
            };
        } else {
            event.r#type = Event_::EventTypes_::kNoteOffEvent as u16;
            event.__field0.noteOff = NoteOffEvent {
                channel: 0,
                pitch: i16::from(key),
                velocity: 0.0,
                noteId: -1,
                tuning: 0.0,
            };
        }
        event
    }

    pub struct Queue {
        pub id: u32,
        pub points: Vec<(i32, f64)>,
    }
    impl Class for Queue {
        type Interfaces = (IParamValueQueue,);
    }
    impl IParamValueQueueTrait for Queue {
        unsafe fn getParameterId(&self) -> ParamID {
            self.id
        }
        unsafe fn getPointCount(&self) -> i32 {
            self.points.len() as i32
        }
        unsafe fn getPoint(&self, index: i32, offset: *mut i32, value: *mut f64) -> tresult {
            let Some(&(o, v)) = self.points.get(index as usize) else {
                return kResultFalse;
            };
            unsafe {
                offset.write(o);
                value.write(v);
            }
            kResultOk
        }
        unsafe fn addPoint(&self, _: i32, _: f64, _: *mut i32) -> tresult {
            kResultFalse
        }
    }

    pub struct Changes(pub Vec<ComWrapper<Queue>>);
    impl Class for Changes {
        type Interfaces = (IParameterChanges,);
    }
    impl IParameterChangesTrait for Changes {
        unsafe fn getParameterCount(&self) -> i32 {
            self.0.len() as i32
        }
        unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
            self.0
                .get(index as usize)
                .and_then(ComWrapper::as_com_ref::<IParamValueQueue>)
                .map_or(std::ptr::null_mut(), |q| q.as_ptr())
        }
        unsafe fn addParameterData(&self, _: *const ParamID, _: *mut i32) -> *mut IParamValueQueue {
            std::ptr::null_mut()
        }
    }

    /// One queue per parameter, points in host order (normalized values).
    pub fn changes(points: &[(u32, i32, f64)]) -> ComWrapper<Changes> {
        let mut queues: Vec<Queue> = Vec::new();
        for &(id, at, norm) in points {
            match queues.iter_mut().find(|q| q.id == id) {
                Some(q) => q.points.push((at, norm)),
                None => queues.push(Queue {
                    id,
                    points: vec![(at, norm)],
                }),
            }
        }
        ComWrapper::new(Changes(queues.into_iter().map(ComWrapper::new).collect()))
    }

    pub fn utf16_to_string(chars: &[u16]) -> String {
        let end = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
        String::from_utf16_lossy(&chars[..end])
    }

    pub struct Vst3 {
        pub component: ComPtr<IComponent>,
        pub processor: ComPtr<IAudioProcessor>,
        pub controller: ComPtr<IEditController>,
        pub active: bool,
        pub processing: bool,
        pub rate: f64,
        pub mode: i32,
    }

    impl Vst3 {
        // VST3 enum constants are u32 on Unix and i32 on Windows.
        #[allow(clippy::unnecessary_cast)]
        pub fn new() -> Self {
            ensure_registered();
            let factory = unsafe {
                ComPtr::<IPluginFactory>::from_raw(moose_vst3::ffi::moose_vst3_get_factory().cast())
            }
            .expect("VST3 factory");
            let mut class: PClassInfo = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { factory.getClassInfo(0, &raw mut class) },
                kResultOk
            );
            let mut obj: *mut c_void = std::ptr::null_mut();
            let result = unsafe {
                factory.createInstance(
                    class.cid.as_ptr(),
                    IComponent::IID.as_ptr().cast(),
                    &raw mut obj,
                )
            };
            assert_eq!(result, kResultOk, "VST3 createInstance");
            let component =
                unsafe { ComPtr::<IComponent>::from_raw(obj.cast()) }.expect("IComponent");
            assert_eq!(
                unsafe { component.initialize(std::ptr::null_mut()) },
                kResultOk
            );
            let processor = component
                .cast::<IAudioProcessor>()
                .expect("IAudioProcessor");
            let controller = component
                .cast::<IEditController>()
                .expect("IEditController");
            Self {
                component,
                processor,
                controller,
                active: false,
                processing: false,
                rate: 48_000.0,
                mode: ProcessModes_::kRealtime as i32,
            }
        }

        // VST3 enum constants are u32 on Unix and i32 on Windows.
        #[allow(clippy::unnecessary_cast)]
        pub fn setup(&mut self, rate: f64) {
            self.rate = rate;
            let mut setup = ProcessSetup {
                processMode: self.mode,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
                maxSamplesPerBlock: MAX_FRAMES as i32,
                sampleRate: rate,
            };
            assert_eq!(
                unsafe { self.processor.setupProcessing(&raw mut setup) },
                kResultOk
            );
        }

        pub fn set_active(&mut self, on: bool) {
            assert_eq!(unsafe { self.component.setActive(u8::from(on)) }, kResultOk);
            self.active = on;
        }

        pub fn set_processing(&mut self, on: bool) {
            assert_eq!(
                unsafe { self.processor.setProcessing(u8::from(on)) },
                kResultOk
            );
            self.processing = on;
        }

        pub fn normalize(&self, id: u32, plain: f64) -> f64 {
            unsafe { self.controller.plainParamToNormalized(id, plain) }
        }

        pub fn denormalize(&self, id: u32, norm: f64) -> f64 {
            unsafe { self.controller.normalizedParamToPlain(id, norm) }
        }

        /// Raw `IAudioProcessor::process`. `buses == false` sends the
        /// Ableton-style suspended flush (zero buses, nonzero samples).
        // VST3 enum constants are u32 on Unix and i32 on Windows.
        #[allow(clippy::unnecessary_cast)]
        pub fn process_raw(
            &self,
            frames: u32,
            buses: bool,
            inputs: &mut [Vec<f32>; 2],
            outputs: &mut [Vec<f32>; 2],
            events: Option<&ComWrapper<Events>>,
            changes: Option<&ComWrapper<Changes>>,
        ) -> tresult {
            let mut in_ptrs = [inputs[0].as_mut_ptr(), inputs[1].as_mut_ptr()];
            let mut out_ptrs = [outputs[0].as_mut_ptr(), outputs[1].as_mut_ptr()];
            let mut in_bus: AudioBusBuffers = unsafe { std::mem::zeroed() };
            in_bus.numChannels = 2;
            in_bus.__field0.channelBuffers32 = in_ptrs.as_mut_ptr();
            let mut out_bus: AudioBusBuffers = unsafe { std::mem::zeroed() };
            out_bus.numChannels = 2;
            out_bus.__field0.channelBuffers32 = out_ptrs.as_mut_ptr();
            let mut data: ProcessData = unsafe { std::mem::zeroed() };
            data.processMode = self.mode;
            data.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as i32;
            data.numSamples = frames as i32;
            if buses {
                data.numInputs = 1;
                data.numOutputs = 1;
                data.inputs = &raw mut in_bus;
                data.outputs = &raw mut out_bus;
            }
            if let Some(events) = events {
                data.inputEvents = events.as_com_ref::<IEventList>().unwrap().as_ptr();
            }
            if let Some(changes) = changes {
                data.inputParameterChanges =
                    changes.as_com_ref::<IParameterChanges>().unwrap().as_ptr();
            }
            unsafe { self.processor.process(&raw mut data) }
        }

        pub fn host_blocks(
            &self,
            events: &[HostEvent],
        ) -> (ComWrapper<Events>, ComWrapper<Changes>) {
            let mut notes = Vec::new();
            let mut points = Vec::new();
            for ev in events {
                match *ev {
                    HostEvent::NoteOn { at, key } => notes.push(note(at, key, true)),
                    HostEvent::NoteOff { at, key } => notes.push(note(at, key, false)),
                    HostEvent::Param { at, id, plain } => {
                        points.push((id, at as i32, self.normalize(id, plain)));
                    }
                }
            }
            (ComWrapper::new(Events(notes)), changes(&points))
        }

        pub fn save_state(&self) -> Option<Vec<u8>> {
            let stream = MemStream::new(Vec::new());
            let ptr = stream.as_com_ref::<IBStream>().unwrap().as_ptr();
            (unsafe { self.component.getState(ptr) } == kResultOk)
                .then(|| stream.data.lock().unwrap().0.clone())
        }

        pub fn load_state(&self, blob: &[u8]) -> bool {
            let stream = MemStream::new(blob.to_vec());
            let ptr = stream.as_com_ref::<IBStream>().unwrap().as_ptr();
            unsafe { self.component.setState(ptr) == kResultOk }
        }
    }

    impl Drop for Vst3 {
        fn drop(&mut self) {
            unsafe {
                if self.processing {
                    self.processor.setProcessing(0);
                }
                if self.active {
                    self.component.setActive(0);
                }
                self.component.terminate();
            }
        }
    }
}

impl Harness for vst3_host_harness::Vst3 {
    const NAME: &'static str = "VST3";
    fn create() -> Self {
        Self::new()
    }
    // VST3 enum constants are u32 on Unix and i32 on Windows.
    #[allow(clippy::unnecessary_cast)]
    fn start(&mut self, rate: f64) {
        use vst3::Steinberg::Vst::{
            BusDirections_::{kInput, kOutput},
            IComponentTrait,
            MediaTypes_::{kAudio, kEvent},
        };
        self.setup(rate);
        for (media, direction) in [(kAudio, kInput), (kAudio, kOutput), (kEvent, kInput)] {
            assert_eq!(
                unsafe {
                    self.component
                        .activateBus(media as i32, direction as i32, 0, 1)
                },
                vst3::Steinberg::kResultOk,
                "VST3 activate bus"
            );
        }
        self.set_active(true);
        self.set_processing(true);
    }
    fn stop(&mut self) {
        self.set_processing(false);
        self.set_active(false);
    }
    fn reset(&mut self) {
        // VST3 has no reset call; hosts cycle processing.
        self.set_processing(false);
        self.set_processing(true);
    }
    fn process(&mut self, frames: u32, input: f32, events: &[HostEvent]) -> [Vec<f32>; 2] {
        let (notes, changes) = self.host_blocks(events);
        let mut inputs = [vec![input; frames as usize], vec![input; frames as usize]];
        let mut outputs = [
            vec![f32::NAN; frames as usize],
            vec![f32::NAN; frames as usize],
        ];
        let result = self.process_raw(
            frames,
            true,
            &mut inputs,
            &mut outputs,
            Some(&notes),
            Some(&changes),
        );
        assert_eq!(result, vst3::Steinberg::kResultOk, "VST3 process");
        outputs
    }
    fn suspended_param_flush(&mut self, events: &[(u32, f64)]) {
        let points: Vec<_> = events
            .iter()
            .map(|&(id, plain)| (id, 0, self.normalize(id, plain)))
            .collect();
        let changes = vst3_host_harness::changes(&points);
        let mut inputs = [Vec::new(), Vec::new()];
        let mut outputs = [Vec::new(), Vec::new()];
        // Live: nonzero numSamples, zero buses.
        let result = self.process_raw(256, false, &mut inputs, &mut outputs, None, Some(&changes));
        assert_eq!(result, vst3::Steinberg::kResultOk, "VST3 suspended flush");
    }
    fn get_plain(&self, id: u32) -> f64 {
        use vst3::Steinberg::Vst::IEditControllerTrait;
        self.denormalize(id, unsafe { self.controller.getParamNormalized(id) })
    }
    fn format(&self, id: u32, plain: f64) -> Option<String> {
        use vst3::Steinberg::Vst::IEditControllerTrait;
        let mut s = [0u16; 128];
        let norm = self.normalize(id, plain);
        (unsafe { self.controller.getParamStringByValue(id, norm, &raw mut s) }
            == vst3::Steinberg::kResultOk)
            .then(|| vst3_host_harness::utf16_to_string(&s))
    }
    fn parse(&self, id: u32, text: &str) -> Option<f64> {
        use vst3::Steinberg::Vst::IEditControllerTrait;
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.resize(256, 0);
        let mut norm = f64::NAN;
        (unsafe {
            self.controller
                .getParamValueByString(id, wide.as_mut_ptr(), &raw mut norm)
        } == vst3::Steinberg::kResultOk)
            .then(|| self.denormalize(id, norm))
    }
    fn save(&self) -> Vec<u8> {
        self.save_state().expect("VST3 getState")
    }
    fn load(&mut self, blob: &[u8]) -> bool {
        self.load_state(blob)
    }
    fn latency(&self) -> u32 {
        use vst3::Steinberg::Vst::IAudioProcessorTrait;
        unsafe { self.processor.getLatencySamples() }
    }
    fn tail(&self) -> u32 {
        use vst3::Steinberg::Vst::IAudioProcessorTrait;
        unsafe { self.processor.getTailSamples() }
    }
    // VST3 enum constants are u32 on Unix and i32 on Windows.
    #[allow(clippy::unnecessary_cast)]
    fn set_offline(&mut self, offline: bool) {
        use vst3::Steinberg::Vst::ProcessModes_;
        self.mode = if offline {
            ProcessModes_::kOffline as i32
        } else {
            ProcessModes_::kRealtime as i32
        };
    }
}

// ---------------------------------------------------------------------------
// AU harness (the shim callback table)
// ---------------------------------------------------------------------------

mod au_host_harness {
    use super::{Contract, HostEvent, MAX_FRAMES};
    use moose_au::ffi::{AuCallbacks, AuMidiEvent, AuParamEvent};
    use std::ffi::{CStr, CString, c_char, c_void};
    use std::ptr;

    pub struct Au {
        pub cb: AuCallbacks,
        pub ctx: *mut c_void,
        pub rate: f64,
    }

    impl Au {
        pub fn new() -> Self {
            let cb = moose_au::callbacks::<Contract>();
            let ctx = unsafe { (cb.create)() };
            assert!(!ctx.is_null(), "AU create");
            Self { cb, ctx, rate: 0.0 }
        }

        pub fn reset_at(&mut self, rate: f64) {
            self.rate = rate;
            unsafe { (self.cb.reset)(self.ctx, rate, MAX_FRAMES) };
        }

        pub fn process_raw(
            &self,
            frames: u32,
            inputs: &[Vec<f32>; 2],
            outputs: &mut [Vec<f32>; 2],
            midi: &[AuMidiEvent],
            params: &[AuParamEvent],
        ) {
            let in_ptrs = [inputs[0].as_ptr(), inputs[1].as_ptr()];
            let mut out_ptrs = [outputs[0].as_mut_ptr(), outputs[1].as_mut_ptr()];
            unsafe {
                (self.cb.process)(
                    self.ctx,
                    in_ptrs.as_ptr(),
                    out_ptrs.as_mut_ptr(),
                    2,
                    2,
                    frames,
                    midi.as_ptr(),
                    midi.len() as u32,
                    ptr::null(),
                    0,
                    params.as_ptr(),
                    params.len() as u32,
                    ptr::null(),
                );
            }
        }

        pub fn host_blocks(events: &[HostEvent]) -> (Vec<AuMidiEvent>, Vec<AuParamEvent>) {
            let mut midi = Vec::new();
            let mut params = Vec::new();
            for ev in events {
                match *ev {
                    HostEvent::NoteOn { at, key } => midi.push(AuMidiEvent {
                        sample_offset: at,
                        status: 0x90,
                        data1: key,
                        data2: 127,
                        port: 0,
                    }),
                    HostEvent::NoteOff { at, key } => midi.push(AuMidiEvent {
                        sample_offset: at,
                        status: 0x80,
                        data1: key,
                        data2: 0,
                        port: 0,
                    }),
                    HostEvent::Param { at, id, plain } => params.push(AuParamEvent {
                        sample_offset: at,
                        param_id: id,
                        value: plain as f32,
                    }),
                }
            }
            (midi, params)
        }

        pub fn format_raw(&self, id: u32, plain: f64) -> Option<String> {
            let mut buf = [0 as c_char; 256];
            let n =
                unsafe { (self.cb.param_format_value)(self.ctx, id, plain, buf.as_mut_ptr(), 256) };
            (n > 0).then(|| {
                unsafe { CStr::from_ptr(buf.as_ptr()) }
                    .to_string_lossy()
                    .into_owned()
            })
        }

        pub fn parse_raw(&self, id: u32, text: &str) -> Option<f64> {
            let text = CString::new(text).unwrap();
            let mut v = f64::NAN;
            (unsafe { (self.cb.param_parse_value)(self.ctx, id, text.as_ptr(), &raw mut v) } != 0)
                .then_some(v)
        }

        pub fn save_state(&self) -> Option<Vec<u8>> {
            let mut data: *mut u8 = ptr::null_mut();
            let mut len = 0u32;
            unsafe { (self.cb.state_save)(self.ctx, &raw mut data, &raw mut len) };
            if data.is_null() {
                return None;
            }
            let blob = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
            unsafe { (self.cb.state_free)(data, len) };
            Some(blob)
        }

        pub fn load_state(&self, blob: &[u8]) -> bool {
            unsafe { (self.cb.state_load)(self.ctx, blob.as_ptr(), blob.len() as u32) != 0 }
        }
    }

    impl Drop for Au {
        fn drop(&mut self) {
            unsafe { (self.cb.destroy)(self.ctx) };
        }
    }
}

impl Harness for au_host_harness::Au {
    const NAME: &'static str = "AU";
    fn create() -> Self {
        Self::new()
    }
    fn start(&mut self, rate: f64) {
        // AudioUnitInitialize -> reset callback (full prepare).
        self.reset_at(rate);
    }
    fn stop(&mut self) {
        // AudioUnitUninitialize has no Rust callback; the instance just
        // stops receiving render calls.
    }
    fn reset(&mut self) {
        // AudioUnitReset at the same configuration.
        let rate = self.rate;
        self.reset_at(rate);
    }
    fn process(&mut self, frames: u32, input: f32, events: &[HostEvent]) -> [Vec<f32>; 2] {
        let (midi, params) = Self::host_blocks(events);
        let inputs = [vec![input; frames as usize], vec![input; frames as usize]];
        let mut outputs = [
            vec![f32::NAN; frames as usize],
            vec![f32::NAN; frames as usize],
        ];
        self.process_raw(frames, &inputs, &mut outputs, &midi, &params);
        outputs
    }
    fn suspended_param_flush(&mut self, events: &[(u32, f64)]) {
        // AU hosts set parameters through AudioUnitSetParameter, which the
        // shim forwards to `param_set_value` whether or not the unit is
        // initialized.
        for &(id, plain) in events {
            unsafe { (self.cb.param_set_value)(self.ctx, id, plain) };
        }
    }
    fn get_plain(&self, id: u32) -> f64 {
        unsafe { (self.cb.param_get_value)(self.ctx, id) }
    }
    fn format(&self, id: u32, plain: f64) -> Option<String> {
        self.format_raw(id, plain)
    }
    fn parse(&self, id: u32, text: &str) -> Option<f64> {
        self.parse_raw(id, text)
    }
    fn save(&self) -> Vec<u8> {
        self.save_state().expect("AU state save")
    }
    fn load(&mut self, blob: &[u8]) -> bool {
        self.load_state(blob)
    }
    fn latency(&self) -> u32 {
        unsafe { (self.cb.latency_samples)(self.ctx) }
    }
    fn tail(&self) -> u32 {
        unsafe { (self.cb.tail_samples)(self.ctx) }
    }
    fn set_offline(&mut self, offline: bool) {
        let mode = if offline {
            ProcessMode::Offline
        } else {
            ProcessMode::Realtime
        };
        unsafe { (self.cb.set_render_mode)(self.ctx, u32::from(mode.as_u8())) };
    }
}

type ClapH = clap_host_harness::Clap;
type Vst3H = vst3_host_harness::Vst3;
type AuH = au_host_harness::Au;

/// Run `scenario` once per format, each on a fresh thread so the
/// fixture's per-thread probes start clean.
macro_rules! for_each_format {
    ($scenario:ident) => {
        mod $scenario {
            #[test]
            fn clap() {
                super::$scenario::<super::ClapH>();
            }
            #[test]
            fn vst3() {
                super::$scenario::<super::Vst3H>();
            }
            #[test]
            fn au() {
                super::$scenario::<super::AuH>();
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Scenario 1: State v2, schema migration, renumbered IDs, fallback isolation
// ---------------------------------------------------------------------------

fn snapshot<H: Harness>(h: &H) -> (f64, f64, f64, f64, String) {
    (
        h.get_plain(MIX),
        h.get_plain(CUTOFF),
        h.get_plain(GAIN),
        h.get_plain(BANK),
        fixture::live().label(),
    )
}

fn state_schema0_v1_blob_migrates_0_to_1_to_2<H: Harness>() {
    let mut h = H::create();
    let _ = fixture::take_migrations();
    // A schema-0 v1 envelope from before the plugin opted into schemas.
    let blob = serialize_state(
        hash(),
        &[GAIN, CUTOFF, MIX, BANK],
        &[0.2, 3000.0, 0.7, 2.0],
        b"v1-extra",
        &ContractParams::persist_blob("v0"),
    );
    assert!(h.load(&blob), "{}: schema-0 v1 blob loads", H::NAME);
    assert_eq!(
        fixture::take_migrations(),
        vec![0, 1],
        "{}: 0 -> 1 -> 2",
        H::NAME
    );
    let (mix, cutoff, gain, bank, label) = snapshot(&h);
    assert!(close(gain, 0.4), "{}: m0 doubled gain, got {gain}", H::NAME);
    assert!(close(cutoff, 3000.0), "{}: cutoff {cutoff}", H::NAME);
    assert!(close(mix, 0.7), "{}: mix {mix}", H::NAME);
    assert_eq!(bank, 2.0, "{}: bank", H::NAME);
    assert_eq!(label, "v0|m0|m1", "{}: persist migrated in order", H::NAME);
}
for_each_format!(state_schema0_v1_blob_migrates_0_to_1_to_2);

fn state_renumbered_string_ids_restore_without_touching_new_owner<H: Harness>() {
    let mut h = H::create();
    // Live values that must survive: `mix` now owns numeric ID 10.
    h.suspended_param_flush(&[(MIX, 0.33), (GAIN, 0.6)]);
    fixture::live().set_label("before");
    let _ = fixture::take_migrations();
    // Schema 1: "cutoff" was saved under its old numeric ID 10. A retired
    // key "mix_old" falls back to numeric 100, which "cutoff" already
    // claimed by string ID, so it must be discarded.
    let blob = serialize_state_v2(
        hash(),
        1,
        &[("cutoff", MIX, 5000.0), ("mix_old", CUTOFF, 0.9)],
        b"",
        &ContractParams::persist_blob("s1"),
    );
    assert!(h.load(&blob), "{}: schema-1 v2 blob loads", H::NAME);
    assert_eq!(
        fixture::take_migrations(),
        vec![1],
        "{}: only 1 -> 2",
        H::NAME
    );
    let (mix, cutoff, gain, _, label) = snapshot(&h);
    assert!(
        close(cutoff, 5000.0),
        "{}: cutoff restored by string ID, got {cutoff}",
        H::NAME
    );
    assert!(
        close(mix, 0.33),
        "{}: mix untouched by stale ID 10, got {mix}",
        H::NAME
    );
    assert!(
        close(gain, 0.6),
        "{}: absent gain untouched, got {gain}",
        H::NAME
    );
    assert_eq!(label, "s1|m1", "{}", H::NAME);
}
for_each_format!(state_renumbered_string_ids_restore_without_touching_new_owner);

fn state_bad_blobs_fail_and_roll_back<H: Harness>() {
    let mut h = H::create();
    h.suspended_param_flush(&[(MIX, 0.11), (CUTOFF, 777.0), (GAIN, 0.22), (BANK, 1.0)]);
    fixture::live().set_label("live");
    let before = snapshot(&h);
    let named = [
        ("mix", MIX, 0.9),
        ("cutoff", CUTOFF, 9000.0),
        ("gain", GAIN, 0.9),
        ("bank", BANK, 2.0),
    ];
    let persist = ContractParams::persist_blob("intruder");

    // A blob from a newer build (schema 9).
    let future = serialize_state_v2(hash(), 9, &named, b"", &persist);
    assert!(!h.load(&future), "{}: future schema refused", H::NAME);
    assert_eq!(
        snapshot(&h),
        before,
        "{}: future schema left state",
        H::NAME
    );

    // Truncated envelope.
    let good = serialize_state_v2(hash(), 2, &named, b"", &persist);
    assert!(
        !h.load(&good[..good.len() - 7]),
        "{}: truncated refused",
        H::NAME
    );
    assert_eq!(snapshot(&h), before, "{}: truncated left state", H::NAME);

    // Unknown envelope version.
    let mut bad_version = good.clone();
    bad_version[4..8].copy_from_slice(&99u32.to_le_bytes());
    assert!(
        !h.load(&bad_version),
        "{}: unknown envelope version",
        H::NAME
    );
    assert_eq!(snapshot(&h), before, "{}: bad version left state", H::NAME);

    // Custom state rejected by `validate_state` up front.
    let invalid = serialize_state_v2(hash(), 2, &named, fixture::INVALID_EXTRA, &persist);
    assert!(!h.load(&invalid), "{}: validate_state refusal", H::NAME);
    assert_eq!(
        snapshot(&h),
        before,
        "{}: validate refusal rolled back",
        H::NAME
    );

    // And the good blob still lands afterwards.
    assert!(h.load(&good), "{}: good blob after failures", H::NAME);
    let (mix, cutoff, gain, bank, label) = snapshot(&h);
    assert_eq!(
        (mix, cutoff, gain, bank, label.as_str()),
        (0.9, 9000.0, 0.9, 2.0, "intruder"),
        "{}",
        H::NAME
    );
}
for_each_format!(state_bad_blobs_fail_and_roll_back);

/// `load_state` rejection on an instance that is not processing: the host
/// thread may own the plugin, so the rejection must reach the host and roll
/// back params + persist.
fn state_load_state_rejection_rolls_back_when_idle<H: Harness>() {
    let mut h = H::create();
    h.suspended_param_flush(&[(MIX, 0.11), (GAIN, 0.22)]);
    fixture::live().set_label("live");
    let before = snapshot(&h);
    let poison = serialize_state_v2(
        hash(),
        2,
        &[("mix", MIX, 0.9), ("gain", GAIN, 0.9)],
        fixture::POISON_EXTRA,
        &ContractParams::persist_blob("intruder"),
    );
    assert!(
        !h.load(&poison),
        "{}: rejected custom state must fail the load",
        H::NAME
    );
    assert_eq!(snapshot(&h), before, "{}: rejection rolled back", H::NAME);
}
// CLAP + VST3 only; AU diverges, see `divergences::au_idle_load_state_rejection`.
mod state_load_state_rejection_rolls_back_when_idle {
    #[test]
    fn clap() {
        super::state_load_state_rejection_rolls_back_when_idle::<super::ClapH>();
    }
    #[test]
    fn vst3() {
        super::state_load_state_rejection_rolls_back_when_idle::<super::Vst3H>();
    }
}

/// On an idle instance (not processing), the custom-state blob reaches
/// `load_state` before the host's load call returns.
fn state_custom_state_applied_before_load_returns_when_idle<H: Harness>() {
    let mut h = H::create();
    let blob = serialize_state_v2(hash(), 2, &[("gain", GAIN, 0.4)], b"session-extra", &[]);
    assert!(h.load(&blob), "{}", H::NAME);
    assert_eq!(
        fixture::LOADED_EXTRA.with(|e| e.borrow().clone()),
        b"session-extra",
        "{}: load_state ran before load returned",
        H::NAME
    );
    // And a save right after returns it.
    let saved = h.save();
    assert!(
        saved
            .windows(b"session-extra".len())
            .any(|w| w == b"session-extra"),
        "{}: save after load carries the loaded custom state",
        H::NAME
    );
}
// CLAP + VST3 only; AU diverges, see `divergences::au_idle_custom_state_deferred`.
mod state_custom_state_applied_before_load_returns_when_idle {
    #[test]
    fn clap() {
        super::state_custom_state_applied_before_load_returns_when_idle::<super::ClapH>();
    }
    #[test]
    fn vst3() {
        super::state_custom_state_applied_before_load_returns_when_idle::<super::Vst3H>();
    }
}

/// A blob saved by one format restores identically in the other two (the
/// envelope hash is shared by design).
#[test]
fn state_saved_by_each_format_restores_identically_in_every_format() {
    fn saved_by<H: Harness>() -> Vec<u8> {
        let mut h = H::create();
        let payload = format!("custom-state-{}", H::NAME);
        let seed = serialize_state_v2(hash(), 2, &[], payload.as_bytes(), &[]);
        assert!(h.load(&seed), "{}: seed custom state", H::NAME);
        h.start(48_000.0);
        let _ = h.process(32, 1.0, &[]);
        h.stop();
        h.suspended_param_flush(&[(MIX, 0.125), (CUTOFF, 4321.0), (GAIN, 0.75), (BANK, 1.0)]);
        fixture::live().set_label(H::NAME);
        let blob = h.save();
        assert_eq!(&blob[0..4], b"OAST", "{}: moose envelope", H::NAME);
        assert_eq!(
            u32::from_le_bytes(blob[4..8].try_into().unwrap()),
            2,
            "{}: schema 2 plugin writes v2",
            H::NAME
        );
        blob
    }
    fn restore<H: Harness>(blob: &[u8], from: &str) {
        let mut h = H::create();
        let _ = fixture::take_migrations();
        fixture::LOADED_EXTRA.with(|e| e.borrow_mut().clear());
        assert!(h.load(blob), "{} <- {from}", H::NAME);
        h.start(48_000.0);
        let _ = h.process(32, 1.0, &[]);
        h.stop();
        assert_eq!(
            fixture::LOADED_EXTRA.with(|e| e.borrow().clone()),
            format!("custom-state-{from}").as_bytes(),
            "{} <- {from}: custom state",
            H::NAME
        );
        assert!(
            fixture::take_migrations().is_empty(),
            "{}: same schema, no migration",
            H::NAME
        );
        assert_eq!(
            snapshot(&h),
            (0.125, 4321.0, 0.75, 1.0, from.to_owned()),
            "{} <- {from}",
            H::NAME
        );
    }
    for (from, blob) in [
        ("CLAP", saved_by::<ClapH>()),
        ("VST3", saved_by::<Vst3H>()),
        ("AU", saved_by::<AuH>()),
    ] {
        restore::<ClapH>(&blob, from);
        restore::<Vst3H>(&blob, from);
        restore::<AuH>(&blob, from);
    }
}

// ---------------------------------------------------------------------------
// Scenario 2: dynamic catalog / ValueListParam lifecycle
// ---------------------------------------------------------------------------

fn value_list_runtime_labels_round_trip<H: Harness>() {
    let h = H::create();
    for (i, label) in fixture::INITIAL_LABELS.iter().enumerate() {
        assert_eq!(
            h.format(BANK, i as f64).as_deref(),
            Some(*label),
            "{}",
            H::NAME
        );
    }
    let params = fixture::live();
    // Same-count relabeling tests live formatting/parsing across all formats;
    // range growth (3 -> 4) is covered separately by
    // `value_list_grown_range_reaches_new_index` and `divergences::vst3_value_list_growth`.
    assert!(params.bank.set_labels(["Sine", "Saw", "Square"]));
    let changes = params.take_catalog_changes();
    // The CLAP wrapper may already have consumed them through its signal;
    // either way the structural change must be visible to someone.
    let _ = changes;
    for (i, label) in ["Sine", "Saw", "Square"].iter().enumerate() {
        assert_eq!(
            h.format(BANK, i as f64).as_deref(),
            Some(*label),
            "{}: format after set_labels",
            H::NAME
        );
        assert_eq!(
            h.parse(BANK, label),
            Some(i as f64),
            "{}: parse {label}",
            H::NAME
        );
    }
    assert_eq!(
        h.parse(BANK, "Wavetable"),
        None,
        "{}: unknown label",
        H::NAME
    );
}
for_each_format!(value_list_runtime_labels_round_trip);

/// The fourth label must be reachable as a host value after `set_labels`.
fn value_list_grown_range_reaches_new_index<H: Harness>() {
    let mut h = H::create();
    let params = fixture::live();
    assert!(params.bank.set_labels(["Sine", "Saw", "Square", "Noise"]));
    h.suspended_param_flush(&[(BANK, 3.0)]);
    assert_eq!(h.get_plain(BANK), 3.0, "{}: index 3 after growth", H::NAME);
    assert_eq!(params.bank.index(), 3, "{}", H::NAME);
}
// CLAP + AU only; VST3 diverges, see `divergences::vst3_value_list_growth`.
mod value_list_grown_range_reaches_new_index {
    #[test]
    fn clap() {
        super::value_list_grown_range_reaches_new_index::<super::ClapH>();
    }
    #[test]
    fn au() {
        super::value_list_grown_range_reaches_new_index::<super::AuH>();
    }
}

#[test]
fn catalog_signal_flags_and_clap_rescan_wake() {
    use clap_sys::ext::params::CLAP_PARAM_RESCAN_ALL;

    // Plain store: set_labels raises STRUCTURE | INFO.
    let au = AuH::new();
    let params = fixture::live();
    assert!(params.bank.set_labels(["A", "B"]));
    let flags = params.take_catalog_changes().flags;
    assert!(
        flags.contains(moose_params::CatalogChange::STRUCTURE),
        "AU store flags"
    );
    assert!(
        flags.contains(moose_params::CatalogChange::INFO),
        "AU store flags"
    );
    assert!(
        params.bank.set_labels(["A", "B"]),
        "identical labels are accepted"
    );
    assert!(
        params.take_catalog_changes().flags.is_empty(),
        "identical labels raise no catalog change"
    );
    drop(au);

    // CLAP: the signal wakes the host through request_callback; the
    // main-thread callback rescans and reports the new range.
    let clap = ClapH::new();
    let before = clap.info_by_id(BANK).expect("bank info");
    assert_eq!(before.max_value, 2.0);
    clap.on_main_thread();
    clap.log
        .rescan_flags
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let callbacks_before = clap
        .log
        .callbacks
        .load(std::sync::atomic::Ordering::Relaxed);
    let params = fixture::live();
    assert!(params.bank.set_labels(["A", "B", "C", "D", "E"]));
    let woke = clap
        .log
        .callbacks
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        woke > callbacks_before,
        "CLAP: catalog change requested a main-thread callback"
    );
    clap.on_main_thread();
    let flags = clap
        .log
        .rescan_flags
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        flags & CLAP_PARAM_RESCAN_ALL != 0,
        "CLAP: rescan flags {flags:#x}"
    );
    let after = clap.info_by_id(BANK).expect("bank info");
    assert_eq!(after.max_value, 4.0, "CLAP get_info sees five labels");
    assert!(
        params.take_catalog_changes().flags.is_empty(),
        "CLAP on_main_thread consumed the catalog changes"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: DAW quirks
// ---------------------------------------------------------------------------

/// Ableton Live suspended flush / CLAP flush while deactivated / AU set
/// parameter while uninitialized: values land, no audio is processed.
fn suspended_flush_updates_params_without_processing<H: Harness>() {
    let mut h = H::create();
    let calls = fixture::PROCESS_CALLS.with(Cell::get);
    h.suspended_param_flush(&[(GAIN, 0.8), (MIX, 0.9)]);
    assert!(
        close(h.get_plain(GAIN), 0.8),
        "{}: gain while inactive",
        H::NAME
    );
    assert!(
        close(h.get_plain(MIX), 0.9),
        "{}: mix while inactive",
        H::NAME
    );
    // Prepared but suspended (Live's device-off case for VST3).
    h.start(48_000.0);
    h.suspended_param_flush(&[(GAIN, 0.3)]);
    assert!(
        close(h.get_plain(GAIN), 0.3),
        "{}: gain while suspended",
        H::NAME
    );
    assert_eq!(
        fixture::PROCESS_CALLS.with(Cell::get),
        calls,
        "{}: no audio processed",
        H::NAME
    );
    // And the next real block uses the flushed value.
    let out = h.process(64, 1.0, &[]);
    assert!(
        out[0].iter().all(|&s| close(f64::from(s), 0.3)),
        "{}: {:?}",
        H::NAME,
        &out[0][..4]
    );
}
for_each_format!(suspended_flush_updates_params_without_processing);

fn assert_block<H: Harness>(out: &[Vec<f32>; 2], expected: f32, step: &str) {
    for (ch, samples) in out.iter().enumerate() {
        for (i, &s) in samples.iter().enumerate() {
            assert!(
                s.is_finite() && (s - expected).abs() < 1e-6,
                "{} [{step}] ch{ch}[{i}] = {s}, expected {expected}",
                H::NAME
            );
        }
    }
}

fn rapid_lifecycle_cycling<H: Harness>() {
    let mut h = H::create();
    h.suspended_param_flush(&[(GAIN, 0.5)]);
    h.start(48_000.0);
    assert_eq!(
        fixture::LAST_RATE.with(Cell::get),
        48_000.0,
        "{}: prepared at 48k",
        H::NAME
    );
    assert_block::<H>(&h.process(128, 1.0, &[]), 0.5, "48k first block");
    let resets = fixture::RESETS.with(Cell::get);
    h.reset();
    assert!(
        fixture::RESETS.with(Cell::get) > resets,
        "{}: explicit reset reached the plugin",
        H::NAME
    );
    assert_block::<H>(&h.process(128, 1.0, &[]), 0.5, "after reset");
    h.stop();
    assert_eq!(h.get_plain(GAIN), 0.5, "{}: state survives stop", H::NAME);

    h.start(96_000.0);
    assert_eq!(
        fixture::LAST_RATE.with(Cell::get),
        96_000.0,
        "{}: re-prepared at 96k",
        H::NAME
    );
    assert_block::<H>(&h.process(MAX_FRAMES, -0.5, &[]), -0.25, "96k max block");
    assert_block::<H>(&h.process(1, 1.0, &[]), 0.5, "single frame");
    h.stop();
    h.start(44_100.0);
    assert_block::<H>(&h.process(64, 1.0, &[]), 0.5, "44.1k");
    assert_eq!(h.latency(), fixture::LATENCY, "{}: latency", H::NAME);
    assert_eq!(h.tail(), fixture::TAIL, "{}: tail", H::NAME);
    h.stop();
    drop(h);
}
for_each_format!(rapid_lifecycle_cycling);

/// Dense notes interleaved with automation, including ties on the same
/// offset. Host order: at a shared offset, notes come before parameter
/// points (VST3 delivers notes and queues separately, so that is the only
/// order all three formats can express).
fn dense_block() -> Vec<HostEvent> {
    let mut events = Vec::new();
    // (offset, key, on): every note-off closes a note opened earlier.
    let notes = [
        (0u32, 40u8, true),
        (3, 41, true),
        (3, 42, true),
        (17, 40, false),
        (64, 43, true),
        (64, 41, false),
        (100, 44, true),
        (128, 42, false),
        (200, 43, false),
        (255, 44, false),
    ];
    let gain = [0u32, 3, 64, 100, 199, 255];
    let mix = [1u32, 64, 254];
    for at in 0..256u32 {
        for &(n, key, on) in &notes {
            if n == at {
                events.push(if on {
                    HostEvent::NoteOn { at, key }
                } else {
                    HostEvent::NoteOff { at, key }
                });
            }
        }
        for (k, &g) in gain.iter().enumerate() {
            if g == at {
                events.push(HostEvent::Param {
                    at,
                    id: GAIN,
                    plain: 0.1 + 0.1 * k as f64,
                });
            }
        }
        for (k, &m) in mix.iter().enumerate() {
            if m == at {
                events.push(HostEvent::Param {
                    at,
                    id: MIX,
                    plain: 0.2 * (k + 1) as f64,
                });
            }
        }
    }
    events
}

fn expected_seen(events: &[HostEvent]) -> Vec<Seen> {
    events
        .iter()
        .map(|e| match *e {
            HostEvent::NoteOn { at, key } => Seen::NoteOn { at, key },
            HostEvent::NoteOff { at, key } => Seen::NoteOff { at, key },
            HostEvent::Param { at, id, .. } => Seen::Param { at, id },
        })
        .collect()
}

fn dense_notes_and_automation_keep_exact_order<H: Harness>() {
    let mut h = H::create();
    h.start(48_000.0);
    let events = dense_block();
    let _ = h.process(256, 1.0, &[]);
    let _ = fixture::take_seen();
    let out = h.process(256, 1.0, &events);
    let seen = fixture::take_seen();
    let params_sent = events
        .iter()
        .filter(|e| matches!(e, HostEvent::Param { .. }))
        .count();
    let params_seen = seen
        .iter()
        .filter(|e| matches!(e, Seen::Param { .. }))
        .count();
    assert_eq!(
        params_seen,
        params_sent,
        "{}: no automation point dropped: {seen:?}",
        H::NAME
    );
    assert!(
        seen.windows(2).all(|w| at_of(&w[0]) <= at_of(&w[1])),
        "{}: sample offsets non-decreasing: {seen:?}",
        H::NAME
    );
    assert_eq!(
        seen,
        expected_seen(&events),
        "{}: exact delivery order",
        H::NAME
    );
    // The last gain point wins by block end.
    assert!(
        close(h.get_plain(GAIN), 0.6),
        "{}: final gain {}",
        H::NAME,
        h.get_plain(GAIN)
    );
    assert!(out[0].iter().all(|s| s.is_finite()), "{}", H::NAME);
}
for_each_format!(dense_notes_and_automation_keep_exact_order);

fn at_of(seen: &Seen) -> u32 {
    match *seen {
        Seen::NoteOn { at, .. } | Seen::NoteOff { at, .. } | Seen::Param { at, .. } => at,
    }
}

/// The audio render path (event ingestion, param application, the plugin's
/// process, output drain) performs zero heap operations on the audio thread.
/// Host-side event objects are built before the measured region.
#[test]
fn zero_audio_thread_allocations_clap() {
    let mut clap = ClapH::new();
    clap.start(48_000.0);
    let events: Vec<_> = dense_block()
        .iter()
        .map(clap_host_harness::ClapEvent::from_host)
        .collect();
    let mut inputs = [vec![1.0; 256], vec![1.0; 256]];
    let mut outputs = [vec![f32::NAN; 256], vec![f32::NAN; 256]];
    fixture::SEEN.with(|_| {});
    let (status, ops) = heap_ops(|| clap.process_raw(256, &mut inputs, &mut outputs, &events));
    assert!(status >= 1);
    for channel in &outputs {
        assert!(
            channel.iter().all(|sample| sample.is_finite()),
            "measured call renders every output sample"
        );
    }
    assert_eq!(ops, 0, "CLAP process heap operations");
}

#[test]
fn zero_audio_thread_allocations_vst3() {
    let mut vst3 = Vst3H::new();
    vst3.start(48_000.0);
    let (notes, changes) = vst3.host_blocks(&dense_block());
    let mut inputs = [vec![1.0; 256], vec![1.0; 256]];
    let mut outputs = [vec![f32::NAN; 256], vec![f32::NAN; 256]];
    fixture::SEEN.with(|_| {});
    let (result, ops) = heap_ops(|| {
        vst3.process_raw(
            256,
            true,
            &mut inputs,
            &mut outputs,
            Some(&notes),
            Some(&changes),
        )
    });
    assert_eq!(result, vst3::Steinberg::kResultOk);
    for channel in &outputs {
        assert!(
            channel.iter().all(|sample| sample.is_finite()),
            "measured call renders every output sample"
        );
    }
    assert_eq!(ops, 0, "VST3 process heap operations");
}

#[test]
fn zero_audio_thread_allocations_au() {
    let mut au = AuH::new();
    au.start(48_000.0);
    let (midi, params) = AuH::host_blocks(&dense_block());
    let inputs = [vec![1.0; 256], vec![1.0; 256]];
    let mut outputs = [vec![0.0; 256], vec![0.0; 256]];
    fixture::SEEN.with(|_| {});
    let ((), ops) = heap_ops(|| au.process_raw(256, &inputs, &mut outputs, &midi, &params));
    assert_eq!(ops, 0, "AU process heap operations");
}

/// The counter itself works (guards against a vacuous zero).
#[test]
fn allocation_counter_detects_heap_operations() {
    let ((), ops) = heap_ops(|| drop(std::hint::black_box(vec![0u8; 32])));
    assert!(ops >= 2);
}

fn render_mode_reaches_process_context<H: Harness>() {
    let mut h = H::create();
    h.set_offline(true);
    h.start(48_000.0);
    let _ = h.process(32, 1.0, &[]);
    assert_eq!(
        fixture::LAST_MODE.with(Cell::get),
        Some(ProcessMode::Offline),
        "{}: offline render",
        H::NAME
    );
    h.stop();
    h.set_offline(false);
    h.start(48_000.0);
    let _ = h.process(32, 1.0, &[]);
    assert_eq!(
        fixture::LAST_MODE.with(Cell::get),
        Some(ProcessMode::Realtime),
        "{}: realtime render",
        H::NAME
    );
}
for_each_format!(render_mode_reaches_process_context);

fn param_text_and_normalization_agree<H: Harness>() {
    let h = H::create();
    assert_eq!(h.get_plain(MIX), 0.25, "{}: mix default", H::NAME);
    assert_eq!(h.get_plain(CUTOFF), 1000.0, "{}: cutoff default", H::NAME);
    assert_eq!(h.get_plain(GAIN), 0.5, "{}: gain default", H::NAME);
    assert_eq!(h.get_plain(BANK), 0.0, "{}: bank default", H::NAME);
    for (id, plain) in [(MIX, 0.75), (CUTOFF, 2500.0), (GAIN, 0.5)] {
        let text = h
            .format(id, plain)
            .unwrap_or_else(|| panic!("{}: format {id}", H::NAME));
        let parsed = h
            .parse(id, &text)
            .unwrap_or_else(|| panic!("{}: parse {text:?}", H::NAME));
        assert!(
            (parsed - plain).abs() <= plain.abs() * 1e-3 + 1e-3,
            "{}: {id} {plain} -> {text:?} -> {parsed}",
            H::NAME
        );
    }
}
for_each_format!(param_text_and_normalization_agree);

/// Every format formats the same plain value to the same text.
#[test]
fn param_text_is_identical_across_formats() {
    let clap = ClapH::new();
    let vst3 = Vst3H::new();
    let au = AuH::new();
    for (id, plain) in [
        (MIX, 0.5),
        (CUTOFF, 2500.0),
        (GAIN, 0.125),
        (BANK, 1.0),
        (BANK, 2.0),
    ] {
        let c = clap.format(id, plain);
        let v = vst3.format(id, plain);
        let a = au.format(id, plain);
        assert!(c.is_some(), "CLAP format {id}");
        assert_eq!(c, v, "CLAP vs VST3 text for {id}={plain}");
        assert_eq!(c, a, "CLAP vs AU text for {id}={plain}");
    }
}

// ---------------------------------------------------------------------------
// Scenario 4: CLAP spec host-harness checks (G1)
// ---------------------------------------------------------------------------

mod clap_spec {
    use super::*;
    use clap_sys::ext::audio_ports::clap_audio_port_info;
    use clap_sys::ext::note_ports::clap_note_port_info;
    use clap_sys::ext::params::CLAP_PARAM_IS_STEPPED;
    use std::ffi::CStr;
    use std::sync::atomic::Ordering;

    fn name(info: &clap_sys::ext::params::clap_param_info) -> String {
        unsafe { CStr::from_ptr(info.name.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn params_count_info_value_text() {
        let clap = ClapH::new();
        let params = clap.params();
        let count = unsafe { (params.count.unwrap())(clap.plugin) };
        assert_eq!(count, 4);
        let infos: Vec<_> = (0..count).map(|i| clap.param_info(i).unwrap()).collect();
        assert!(clap.param_info(count).is_none(), "out-of-range index");
        let ids: Vec<u32> = infos.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![MIX, CUTOFF, GAIN, BANK]);
        let names: Vec<String> = infos.iter().map(name).collect();
        assert_eq!(names, ["Mix", "Cutoff", "Gain", "Bank"]);
        let cutoff = &infos[1];
        assert_eq!(
            (cutoff.min_value, cutoff.max_value, cutoff.default_value),
            (20.0, 20000.0, 1000.0)
        );
        let bank = &infos[3];
        assert_eq!((bank.min_value, bank.max_value), (0.0, 2.0));
        assert!(
            bank.flags & CLAP_PARAM_IS_STEPPED != 0,
            "value list is stepped"
        );
        for info in &infos {
            assert!(info.cookie.is_null(), "moose hands out no cookies");
            let mut v = f64::NAN;
            assert!(unsafe { (params.get_value.unwrap())(clap.plugin, info.id, &raw mut v) });
            assert_eq!(v, info.default_value, "{} default", name(info));
        }
        let mut v = 0.0;
        assert!(
            !unsafe { (params.get_value.unwrap())(clap.plugin, 4242, &raw mut v) },
            "unknown id"
        );
        assert_eq!(clap.format(BANK, 1.0).as_deref(), Some("Warm"));
        assert_eq!(clap.parse(BANK, "Bright"), Some(2.0));
        assert_eq!(clap.format(4242, 0.0), None, "unknown id text");
        // Too-small buffer must not overflow.
        let mut tiny = [0x55 as std::ffi::c_char; 8];
        let _ = unsafe {
            (params.value_to_text.unwrap())(clap.plugin, CUTOFF, 1234.5, tiny.as_mut_ptr(), 2)
        };
        assert_eq!(tiny[1], 0, "NUL-terminated in capacity");
        assert!(
            tiny[2..].iter().all(|&byte| byte == 0x55),
            "capacity sentinel"
        );
    }

    #[test]
    fn params_flush_with_cookies_inactive_and_active() {
        let mut clap = ClapH::new();
        let cookie = clap.info_by_id(GAIN).unwrap().cookie;
        // Spec: pass back the cookie get_info handed out.
        let events = vec![
            clap_host_harness::ClapEvent::param(0, GAIN, 0.7, cookie),
            clap_host_harness::ClapEvent::param(0, BANK, 2.0, cookie),
        ];
        clap.flush(&events);
        assert_eq!(clap.get_plain(GAIN), 0.7);
        assert_eq!(clap.get_plain(BANK), 2.0);
        // A host that passes a foreign cookie must not be trusted with it:
        // moose advertises null cookies, so anything else is ignored.
        let mut foreign = 0xdead_beef_u64;
        let events = vec![clap_host_harness::ClapEvent::param(
            0,
            MIX,
            0.6,
            (&raw mut foreign).cast(),
        )];
        clap.flush(&events);
        assert_eq!(clap.get_plain(MIX), 0.6);
        // Invalid values are dropped.
        let events = vec![
            clap_host_harness::ClapEvent::param(0, MIX, f64::NAN, std::ptr::null_mut()),
            clap_host_harness::ClapEvent::param(0, 4242, 0.1, std::ptr::null_mut()),
        ];
        clap.flush(&events);
        assert_eq!(clap.get_plain(MIX), 0.6, "NaN dropped");
        // Active but not processing: flush still applies.
        assert!(clap.activate(48_000.0));
        clap.flush(&vec![clap_host_harness::ClapEvent::param(
            0,
            GAIN,
            0.2,
            std::ptr::null_mut(),
        )]);
        assert_eq!(clap.get_plain(GAIN), 0.2);
        assert_eq!(
            fixture::PROCESS_CALLS.with(Cell::get),
            0,
            "flush never processes"
        );
    }

    #[test]
    fn state_latency_tail_ports_render() {
        let clap = ClapH::new();
        // state
        let blob = clap.save_state().expect("save");
        assert!(clap.load_state(&blob));
        assert!(!clap.load_state(b""), "empty stream");
        assert!(
            !clap.load_state(b"not a moose blob at all"),
            "foreign bytes"
        );
        // latency / tail
        assert_eq!(
            unsafe { (clap.latency_ext().get.unwrap())(clap.plugin) },
            fixture::LATENCY
        );
        assert_eq!(
            unsafe { (clap.tail_ext().get.unwrap())(clap.plugin) },
            fixture::TAIL
        );
        // audio ports
        let ports = clap.audio_ports();
        for is_input in [true, false] {
            assert_eq!(unsafe { (ports.count.unwrap())(clap.plugin, is_input) }, 1);
            let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
            assert!(unsafe { (ports.get.unwrap())(clap.plugin, 0, is_input, &raw mut info) });
            assert_eq!(info.channel_count, 2);
            assert!(!unsafe { (ports.get.unwrap())(clap.plugin, 1, is_input, &raw mut info) });
        }
        // note ports
        let notes = clap.note_ports();
        assert_eq!(unsafe { (notes.count.unwrap())(clap.plugin, true) }, 1);
        assert_eq!(unsafe { (notes.count.unwrap())(clap.plugin, false) }, 0);
        let mut info: clap_note_port_info = unsafe { std::mem::zeroed() };
        assert!(unsafe { (notes.get.unwrap())(clap.plugin, 0, true, &raw mut info) });
        assert!(info.supported_dialects != 0);
        // render
        let render = clap.render();
        assert!(!unsafe { (render.has_hard_realtime_requirement.unwrap())(clap.plugin) });
        assert!(clap.set_render(true));
        assert!(clap.set_render(false));
    }

    #[test]
    fn host_requests_and_main_thread_wakes() {
        let mut clap = ClapH::new();
        assert!(clap.activate(48_000.0));
        assert!(clap.start_processing());
        // Drain startup work so only this state recall can satisfy the wake.
        clap.on_main_thread();
        let callbacks_before = clap.log.callbacks.load(Ordering::Relaxed);
        let rescans_before = clap.log.rescans.load(Ordering::Relaxed);
        clap.log.rescan_flags.store(0, Ordering::Relaxed);
        let blob = serialize_state_v2(hash(), 2, &[("gain", GAIN, 0.9)], b"", &[]);
        let done = std::sync::atomic::AtomicBool::new(false);
        let started = std::sync::atomic::AtomicBool::new(false);
        let loader = std::thread::scope(|s| {
            // Only the render role crosses threads; all main-thread ABI
            // calls stay on the thread that created the instance.
            struct AudioThread<'a>(&'a ClapH);
            unsafe impl Send for AudioThread<'_> {}
            struct StopOnDrop<'a>(&'a std::sync::atomic::AtomicBool);
            impl Drop for StopOnDrop<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            impl AudioThread<'_> {
                fn render_until(
                    &self,
                    started: &std::sync::atomic::AtomicBool,
                    done: &std::sync::atomic::AtomicBool,
                ) {
                    let mut inputs = [vec![1.0; 64], vec![1.0; 64]];
                    let mut outputs = [vec![0.0; 64], vec![0.0; 64]];
                    let events = Vec::new();
                    while !done.load(Ordering::Acquire) {
                        let status = self.0.process_raw(64, &mut inputs, &mut outputs, &events);
                        assert!(status >= 1);
                        started.store(true, Ordering::Release);
                        std::thread::yield_now();
                    }
                }
            }
            let audio = AudioThread(&clap);
            let done_ref = &done;
            let started_ref = &started;
            let handle = s.spawn(move || audio.render_until(started_ref, done_ref));
            let _stop_render = StopOnDrop(&done);
            // Exercise rendering before recall; overlap with the load remains
            // timing-dependent. A worker panic must also release this wait.
            while !started.load(Ordering::Acquire) && !handle.is_finished() {
                std::thread::yield_now();
            }
            let loaded = clap.load_state(&blob);
            done.store(true, Ordering::Release);
            handle.join().unwrap();
            loaded
        });
        assert!(loader, "load while processing");
        assert_eq!(clap.get_plain(GAIN), 0.9);
        assert!(
            clap.log.callbacks.load(Ordering::Relaxed) > callbacks_before,
            "state recall requests a callback for the host value rescan"
        );
        clap.on_main_thread();
        assert!(clap.log.rescans.load(Ordering::Relaxed) > rescans_before);
        assert_ne!(
            clap.log.rescan_flags.load(Ordering::Relaxed)
                & clap_sys::ext::params::CLAP_PARAM_RESCAN_VALUES,
            0
        );
        assert_eq!(clap.get_plain(GAIN), 0.9);
        clap.stop_processing();
        clap.deactivate();
    }

    #[test]
    fn rejects_invalid_process_shapes() {
        let mut clap = ClapH::new();
        clap.start(48_000.0);
        // Moose treats zero frames as a parameter flush. This is a
        // wrapper-specific extension outside the activated [1, MAX] range.
        let calls = fixture::PROCESS_CALLS.with(Cell::get);
        let mut empty_inputs = [Vec::new(), Vec::new()];
        let mut empty_outputs = [Vec::new(), Vec::new()];
        assert_eq!(
            clap.process_raw(
                0,
                &mut empty_inputs,
                &mut empty_outputs,
                &vec![clap_host_harness::ClapEvent::param(
                    0,
                    GAIN,
                    0.875,
                    std::ptr::null_mut()
                )]
            ),
            clap_sys::process::CLAP_PROCESS_CONTINUE
        );
        assert_eq!(fixture::PROCESS_CALLS.with(Cell::get), calls);
        assert_eq!(clap.get_plain(GAIN), 0.875);
        // Frames beyond the activated maximum are refused, not overrun.
        let mut inputs = [vec![1.0; 1024], vec![1.0; 1024]];
        let mut outputs = [vec![0.0; 1024], vec![0.0; 1024]];
        let events = Vec::new();
        let calls = fixture::PROCESS_CALLS.with(Cell::get);
        let status = clap.process_raw(1024, &mut inputs, &mut outputs, &events);
        assert_eq!(status, clap_sys::process::CLAP_PROCESS_ERROR);
        assert_eq!(fixture::PROCESS_CALLS.with(Cell::get), calls);
    }
}

// ---------------------------------------------------------------------------
// Divergences found by this suite. Each is `#[ignore]`d with the wrapper
// track that owns the fix; run with `--ignored` to reproduce.
// ---------------------------------------------------------------------------

mod divergences {
    use super::*;

    /// AU: a custom-state rejection from `load_state` on an instance that is
    /// not rendering is reported to the host as success.
    #[test]
    #[ignore = "moose-au: idle state load queues for a render thread that never runs (see report)"]
    fn au_idle_load_state_rejection() {
        state_load_state_rejection_rolls_back_when_idle::<AuH>();
    }

    /// VST3: a `ValueListParam` that grows at runtime keeps its
    /// registration-time step count.
    #[test]
    #[ignore = "moose-vst3: value-list range is fixed at registration (see report)"]
    fn vst3_value_list_growth() {
        use vst3::Steinberg::Vst::IEditControllerTrait;
        let mut h = Vst3H::new();
        let step_count = |h: &Vst3H| {
            (0..unsafe { h.controller.getParameterCount() })
                .find_map(|index| {
                    let mut info =
                        unsafe { std::mem::zeroed::<vst3::Steinberg::Vst::ParameterInfo>() };
                    assert_eq!(
                        unsafe { h.controller.getParameterInfo(index, &raw mut info) },
                        vst3::Steinberg::kResultOk
                    );
                    (info.id == BANK).then_some(info.stepCount)
                })
                .expect("BANK metadata")
        };
        assert_eq!(step_count(&h), 2);
        let params = fixture::live();
        assert!(params.bank.set_labels(["Sine", "Saw", "Square", "Noise"]));
        assert_eq!(step_count(&h), 3);
        h.suspended_param_flush(&[(BANK, 3.0)]);
        assert_eq!(h.get_plain(BANK), 3.0, "VST3: index 3 after growth");
        assert_eq!(params.bank.index(), 3);
    }
}
