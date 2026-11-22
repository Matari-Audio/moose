//! Shared C-ABI driver primitives for format wrappers (`moose-au`, `moose-aax`).
//!
//! Centralizes the format-agnostic process, state streaming, parameter text
//! formatting/parsing, lock-free `SysEx` input handoff, off-UI-thread editor
//! gesture queue, and single-main + optional-sidechain bus routing so C/C++
//! ABI wrappers stay strictly thin translators without duplicating driver code.

use std::collections::VecDeque;
use std::ffi::{CStr, c_void};
use std::os::raw::c_char;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::buffer::RawBufferScratch;
use crate::bus_routing::{BusActivation, BusRouting};
use crate::cast::len_u32;
use crate::chunked_process::{ChunkedProcess, process_chunked_with_bus_routing_and_catalog};
use crate::config::ProcessMode;
use crate::delay::ReportedDelay;
use crate::events::{EventList, ExactEvent, ExactEventBody, SYSEX_POOL_PREALLOC, TransportInfo};
use crate::export::PluginExport;
use crate::host_queue::HostQueue;
use crate::info::Automation;
use crate::plugin::PluginRuntime;
use crate::snapshot::SnapshotSlot;
use crate::state::{
    self, DeserializedState, ForeignState, ParamsBackup, PluginFormat, StateLoadQueue,
};
use crate::wrapper::{PluginCell, copy_c_str, custom_state_for_save};
use moose_params::{ParamCatalog, Params};

// ---------------------------------------------------------------------------
// Lock-free SysEx input handoff (host thread -> audio thread)
// ---------------------------------------------------------------------------

/// One `SysEx` message handed from a host thread to the audio thread.
/// Recycled through a free-list ([`SysExQueue`]) so the audio thread never
/// allocates or frees: it copies the bytes into the event pool and returns
/// the buffer.
pub struct SysExInput {
    pub sample_offset: u32,
    pub bytes: Vec<u8>,
}

impl Default for SysExInput {
    fn default() -> Self {
        Self {
            sample_offset: 0,
            bytes: Vec::with_capacity(SYSEX_POOL_PREALLOC),
        }
    }
}

/// Lock-free `SysEx` input handoff queue.
pub type SysExQueue = crossbeam_queue::ArrayQueue<SysExInput>;

/// Number of preallocated `SysEx` input buffers cycling between `free` and `ready`.
pub const SYSEX_INPUT_SLOTS: usize = 64;

/// Allocate a paired `(ready, free)` [`SysExQueue`] pre-populated with
/// [`SYSEX_INPUT_SLOTS`] buffers.
#[must_use]
pub fn new_sysex_queues() -> (SysExQueue, SysExQueue) {
    let ready = SysExQueue::new(SYSEX_INPUT_SLOTS);
    let free = SysExQueue::new(SYSEX_INPUT_SLOTS);
    for _ in 0..SYSEX_INPUT_SLOTS {
        let _ = free.push(SysExInput::default());
    }
    (ready, free)
}

/// Host-thread side of the `SysEx` input handoff: copy one message into a
/// recycled buffer and queue it. Returns `false` if no buffer is free or the
/// message exceeds [`SYSEX_POOL_PREALLOC`].
#[must_use]
pub fn try_queue_sysex_input(
    free: &SysExQueue,
    ready: &SysExQueue,
    sample_offset: u32,
    bytes: &[u8],
) -> bool {
    let Some(mut slot) = free.pop() else {
        return false;
    };
    if bytes.len() > slot.bytes.capacity() {
        let _ = free.push(slot);
        return false;
    }
    slot.sample_offset = sample_offset;
    slot.bytes.clear();
    slot.bytes.extend_from_slice(bytes);
    if let Err(slot) = ready.push(slot) {
        let _ = free.push(slot);
        return false;
    }
    true
}

/// Best-effort wrapper over [`try_queue_sysex_input`] for hosts without
/// backpressure reporting.
pub fn queue_sysex_input(free: &SysExQueue, ready: &SysExQueue, sample_offset: u32, bytes: &[u8]) {
    let _ = try_queue_sysex_input(free, ready, sample_offset, bytes);
}

/// Audio-thread side: drain every queued `SysEx` into `event_list`, returning
/// each buffer to `free` without allocating or deallocating.
pub fn drain_sysex_input(ready: &SysExQueue, free: &SysExQueue, event_list: &mut EventList) {
    while let Some(slot) = ready.pop() {
        let exact = ExactEvent::new(slot.sample_offset, ExactEventBody::SysEx { port: 0 });
        let _ =
            event_list.try_push_sysex_with_exact_on_port(slot.sample_offset, 0, &slot.bytes, exact);
        let _ = free.push(slot);
    }
}

// ---------------------------------------------------------------------------
// Off-UI-thread editor gesture queue
// ---------------------------------------------------------------------------

/// Queued off-UI-thread editor gesture delivered on the host main/UI thread.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EditorGesture {
    BeginEdit(u32),
    SetParam(u32, f32),
    EndEdit(u32),
}

/// Bounded editor gesture queue that reserves one slot for each open
/// [`EditorGesture::BeginEdit`] so its matching [`EditorGesture::EndEdit`]
/// can never be dropped under flood.
#[derive(Default)]
pub struct GestureQueue {
    pub pending: VecDeque<EditorGesture>,
    reserved: Vec<u32>,
}

impl GestureQueue {
    /// Push a gesture into the queue, coalescing back-to-back `SetParam`
    /// updates for the same parameter when at capacity.
    pub fn push(&mut self, gesture: EditorGesture) {
        let used = self.pending.len() + self.reserved.len();
        match gesture {
            EditorGesture::BeginEdit(id) if used + 2 <= 256 => self.reserved.push(id),
            EditorGesture::EndEdit(id) => {
                let Some(i) = self.reserved.iter().position(|&open| open == id) else {
                    return;
                };
                self.reserved.swap_remove(i);
            }
            EditorGesture::SetParam(_, _) if used < 256 => {}
            EditorGesture::SetParam(id, value) => {
                if let Some(EditorGesture::SetParam(last_id, last_value)) = self.pending.back_mut()
                    && *last_id == id
                {
                    *last_value = value;
                }
                return;
            }
            EditorGesture::BeginEdit(_) => return,
        }
        self.pending.push_back(gesture);
    }
}

// ---------------------------------------------------------------------------
// Single-main + optional-sidechain bus routing & raw output zeroing
// ---------------------------------------------------------------------------

/// Build a [`BusRouting`] for formats that route one main input bus, at most
/// one sidechain input bus (index 1), and one main output bus (AU and AAX).
/// Any additional declared buses stay visible as zero-width inactive routes.
#[must_use]
pub fn build_single_sidechain_bus_routing(
    input_bus_count: usize,
    output_bus_count: usize,
    num_input_channels: u32,
    num_output_channels: u32,
    declared_sidechain_channels: u32,
    bus_active: Option<(u32, u32)>,
) -> BusRouting {
    let sidechain_channels = declared_sidechain_channels.min(num_input_channels);
    let main_input_channels = num_input_channels.saturating_sub(sidechain_channels);
    let activation = |direction_mask: Option<u32>, bus: usize| match direction_mask {
        Some(mask) if mask & (1_u32 << bus) == 0 => BusActivation::Inactive,
        Some(_) => BusActivation::Active,
        None => BusActivation::Unknown,
    };
    let mut bus_routing = BusRouting::new();
    for index in 0..input_bus_count {
        let channels = match index {
            0 => main_input_channels,
            1 => sidechain_channels,
            _ => 0,
        };
        let state = if channels == 0 {
            BusActivation::Inactive
        } else {
            activation(bus_active.map(|m| m.0), index)
        };
        let pushed = bus_routing.push_input(channels, state);
        debug_assert!(pushed);
    }
    for index in 0..output_bus_count {
        let channels = if index == 0 { num_output_channels } else { 0 };
        let state = if channels == 0 {
            BusActivation::Inactive
        } else {
            activation(bus_active.map(|m| m.1), index)
        };
        let pushed = bus_routing.push_output(channels, state);
        debug_assert!(pushed);
    }
    bus_routing
}

/// Zero `num_output_channels` non-null host float buffers of `num_frames` each.
///
/// # Safety
/// `outputs` must point to at least `num_output_channels` pointers, each
/// either null or valid for `num_frames` writable `f32` samples.
pub unsafe fn zero_raw_outputs(
    outputs: *mut *mut f32,
    num_output_channels: u32,
    num_frames: usize,
) {
    if outputs.is_null() {
        return;
    }
    for ch in 0..num_output_channels as usize {
        let ptr = unsafe { *outputs.add(ch) };
        if !ptr.is_null() {
            unsafe { std::ptr::write_bytes(ptr, 0, num_frames) };
        }
    }
}

/// Arguments for [`run_c_abi_audio_block`].
pub struct CAbiAudioBlock<'a, P: PluginExport> {
    pub plugin: &'a mut P,
    pub params: &'a P::Params,
    pub scratch: &'a mut RawBufferScratch<<P as PluginRuntime>::Sample>,
    pub inputs: *const *const f32,
    pub outputs: *mut *mut f32,
    pub num_input_channels: u32,
    pub num_output_channels: u32,
    pub num_frames: u32,
    pub event_list: &'a EventList,
    pub sub_event_scratch: &'a mut EventList,
    pub output_events: &'a mut EventList,
    pub transport: TransportInfo,
    pub sample_rate: f64,
    pub process_mode: ProcessMode,
    pub catalog: &'a ParamCatalog,
    pub automation: Automation,
    pub bus_routing: BusRouting,
    pub delay: &'a ReportedDelay,
    pub host_queue: &'a HostQueue,
}

/// Execute one chunked audio block over raw host `f32` channel pointers,
/// handling `f64` widening/narrowing, output event sorting, and event-driven
/// `ReportedDelay` signalling.
///
/// # Safety
/// `block.inputs` and `block.outputs` must be valid for the declared channel
/// and frame counts for the duration of the call.
pub unsafe fn run_c_abi_audio_block<P: PluginExport>(block: &mut CAbiAudioBlock<'_, P>) {
    let mut audio_buffer = unsafe {
        block.scratch.build(
            block.inputs,
            block.outputs,
            block.num_input_channels,
            block.num_output_channels,
            block.num_frames,
            P::supports_in_place(),
        )
    };
    let mut transport_snap = block.transport;
    let chunk_args = ChunkedProcess {
        events: block.event_list,
        sub_event_scratch: block.sub_event_scratch,
        transport: &mut transport_snap,
        sample_rate: block.sample_rate,
        process_mode: block.process_mode,
        output_events: block.output_events,
        params_fn: None,
        meters_fn: None,
        latency_change_request: None,
        param_infos: block.catalog,
        automation: block.automation,
    };
    process_chunked_with_bus_routing_and_catalog(
        block.plugin,
        block.params as &dyn Params,
        &mut audio_buffer,
        chunk_args,
        block.bus_routing,
        block.catalog,
    );
    let _ = audio_buffer;
    unsafe {
        block
            .scratch
            .finish_widening(block.outputs, block.num_output_channels, block.num_frames);
    }
    block.output_events.ensure_sorted_by_offset();
    block.delay.observe_and_signal(
        block.plugin.latency(),
        block.plugin.tail(),
        block.host_queue,
    );
}

// ---------------------------------------------------------------------------
// C-ABI parameter text format / parse
// ---------------------------------------------------------------------------

/// Format a parameter value into a host-supplied C string buffer. Returns the
/// number of UTF-8 content bytes written (excluding NUL), or `0` on failure.
///
/// # Safety
/// When `out` is non-null and `out_len > 0`, `out` must point to at least
/// `out_len` writable bytes.
#[must_use]
pub unsafe fn format_param_to_c_buf(
    params: &dyn Params,
    id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) -> u32 {
    if out_len == 0 || out.is_null() {
        return 0;
    }
    match params.format_value(id, value) {
        Some(text) => len_u32(unsafe { copy_c_str(out, out_len as usize, &text) }),
        None => 0,
    }
}

/// Parse a NUL-terminated UTF-8 host string into a plain parameter value.
/// Returns `1` and writes `*out_plain` on success, `0` otherwise.
///
/// # Safety
/// `text` must be null or a valid NUL-terminated C string; `out_plain` must be
/// null or writable.
#[must_use]
pub unsafe fn parse_param_from_c_str(
    params: &dyn Params,
    id: u32,
    text: *const c_char,
    out_plain: *mut f64,
) -> i32 {
    if text.is_null() || out_plain.is_null() {
        return 0;
    }
    let Ok(text) = unsafe { CStr::from_ptr(text) }.to_str() else {
        return 0;
    };
    match params.parse_value(id, text) {
        Some(v) => {
            unsafe { *out_plain = v };
            1
        }
        None => 0,
    }
}

// ---------------------------------------------------------------------------
// C-ABI state streaming (save / load / foreign migration)
// ---------------------------------------------------------------------------

/// Stream the plugin's full OAST state envelope into a single contiguous
/// buffer allocated via `alloc_fn` (typically libc `malloc`), rolling back via
/// `free_fn` on any error or unwind. Returns `Some((ptr, len))` on success.
///
/// # Safety
/// `alloc_fn` must return a writable buffer of at least `len` bytes (or null),
/// and `free_fn` must accept pointers returned by `alloc_fn`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn save_state_to_host_buf<P: PluginExport>(
    plugin: &PluginCell<P>,
    params: &Arc<P::Params>,
    snapshot: &SnapshotSlot,
    pending_state: &StateLoadQueue,
    plugin_id_hash: u64,
    can_own: bool,
    alloc_fn: unsafe extern "C" fn(usize) -> *mut c_void,
    free_fn: unsafe extern "C" fn(*mut c_void),
) -> Option<(*mut u8, u32)> {
    struct Guard {
        ptr: *mut u8,
        free_fn: unsafe extern "C" fn(*mut c_void),
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            if !self.ptr.is_null() {
                unsafe { (self.free_fn)(self.ptr.cast()) };
            }
        }
    }

    let (ids, values) = params.collect_values();
    let extra = custom_state_for_save(plugin, snapshot, pending_state, can_own);
    let persist = params.prepare_persist_write().ok()?;
    let len = state::plugin_state_stream_size::<P>(
        params.as_ref(),
        &ids,
        &values,
        &extra,
        persist.byte_len(),
        P::max_host_state_bytes(),
    )?;
    let host_len = u32::try_from(len).ok()?;
    let mut guard = Guard {
        ptr: unsafe { alloc_fn(len) }.cast::<u8>(),
        free_fn,
    };
    if guard.ptr.is_null() {
        return None;
    }
    let mut offset = 0usize;
    let ok = state::write_plugin_state_stream::<P>(
        plugin_id_hash,
        params.as_ref(),
        &ids,
        &values,
        &extra,
        persist.byte_len(),
        P::max_host_state_bytes(),
        |output| moose_params::write_prepared_persist(persist.as_ref(), output),
        |bytes| {
            if bytes.len() > len - offset {
                return -1;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), guard.ptr.add(offset), bytes.len());
            }
            offset += bytes.len();
            i64::try_from(bytes.len()).unwrap_or(-1)
        },
    );
    if !ok {
        return None;
    }
    let ptr = guard.ptr;
    guard.ptr = std::ptr::null_mut();
    Some((ptr, host_len))
}

/// Parse a host state blob, migrate it if needed, and apply parameters
/// atomically while returning `(DeserializedState, ParamsBackup)` for custom
/// state commit/rollback.
#[must_use]
pub fn load_state_from_host_slice<P: PluginExport>(
    mut blob: &[u8],
    plugin_id_hash: u64,
    format: PluginFormat,
    source_key: Option<&str>,
    params: &P::Params,
) -> Option<(DeserializedState, ParamsBackup)> {
    if blob.is_empty() {
        return None;
    }
    let len = blob.len();
    let backup = ParamsBackup::capture(params);
    let deserialized = state::read_and_apply_params::<P>(
        |dst| std::io::Read::read(&mut blob, dst).map_or(-1, |n| i64::try_from(n).unwrap_or(-1)),
        plugin_id_hash,
        format,
        source_key,
        params,
        len,
    )?;
    Some((deserialized, backup))
}

/// Offer a foreign-format state blob to `P::migrate_state`, applying migrated
/// parameters atomically while returning `(DeserializedState, ParamsBackup)`.
#[must_use]
pub fn load_foreign_state_from_host_slice<P: PluginExport>(
    format: PluginFormat,
    key: Option<&str>,
    bytes: &[u8],
    params: &P::Params,
) -> Option<(DeserializedState, ParamsBackup)> {
    if bytes.is_empty() {
        return None;
    }
    let migrated = P::migrate_state(&ForeignState::Raw {
        format,
        source_key: key,
        bytes,
    })?;
    let deserialized: DeserializedState = migrated.into();
    let backup = ParamsBackup::capture(params);
    state::apply_params(params, &deserialized);
    Some((deserialized, backup))
}

/// Helper to read a `ProcessMode` stored in an `AtomicU8`.
#[inline]
#[must_use]
pub fn load_render_mode(atom: &std::sync::atomic::AtomicU8) -> ProcessMode {
    ProcessMode::from_u8(atom.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBody;

    #[test]
    fn sysex_queue_round_trip_and_recycle() {
        let (ready, free) = new_sysex_queues();
        assert_eq!(free.len(), SYSEX_INPUT_SLOTS);
        assert!(ready.is_empty());

        let payload = [0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7];
        assert!(try_queue_sysex_input(&free, &ready, 12, &payload));
        assert_eq!(free.len(), SYSEX_INPUT_SLOTS - 1);
        assert_eq!(ready.len(), 1);

        let mut events = EventList::with_capacity(16);
        drain_sysex_input(&ready, &free, &mut events);
        assert_eq!(free.len(), SYSEX_INPUT_SLOTS);
        assert!(ready.is_empty());
        assert_eq!(events.len(), 1);
        let ev = events.get(0).unwrap();
        assert_eq!(ev.sample_offset, 12);
        match ev.body {
            EventBody::SysEx { .. } => {
                assert_eq!(events.sysex_bytes(&ev.body), &payload);
            }
            other => panic!("expected SysEx, got {other:?}"),
        }
    }

    #[test]
    fn gesture_queue_reserves_headroom_and_coalesces_at_capacity() {
        let mut queue = GestureQueue::default();
        queue.push(EditorGesture::BeginEdit(1));
        for i in 0_u16..300 {
            queue.push(EditorGesture::SetParam(1, f32::from(i)));
        }
        queue.push(EditorGesture::EndEdit(1));

        assert_eq!(queue.pending.front(), Some(&EditorGesture::BeginEdit(1)));
        assert_eq!(queue.pending.back(), Some(&EditorGesture::EndEdit(1)));
        let penultimate = queue.pending[queue.pending.len() - 2];
        assert_eq!(penultimate, EditorGesture::SetParam(1, 299.0));
    }

    #[test]
    fn single_sidechain_bus_routing_splits_main_and_aux() {
        let routing = build_single_sidechain_bus_routing(2, 1, 4, 2, 2, Some((0b11, 0b1)));
        assert_eq!(routing.input_count(), 2);
        assert_eq!(routing.output_count(), 1);
        let in0 = routing.input(0).unwrap();
        let in1 = routing.input(1).unwrap();
        assert_eq!(in0.channel_range(), 0..2);
        assert_eq!(in0.activation(), BusActivation::Active);
        assert_eq!(in1.channel_range(), 2..4);
        assert_eq!(in1.activation(), BusActivation::Active);

        let inactive_sc = build_single_sidechain_bus_routing(2, 1, 2, 2, 0, Some((0b01, 0b1)));
        let sc1 = inactive_sc.input(1).unwrap();
        assert_eq!(sc1.channel_count(), 0);
        assert_eq!(sc1.activation(), BusActivation::Inactive);
    }

    #[test]
    fn zero_raw_outputs_handles_null_and_fills_zero() {
        unsafe {
            zero_raw_outputs(std::ptr::null_mut(), 2, 8);
        }
        let mut ch0 = [1.0f32; 4];
        let mut ch1 = [2.0f32; 4];
        let mut ptrs = [ch0.as_mut_ptr(), ch1.as_mut_ptr()];
        unsafe {
            zero_raw_outputs(ptrs.as_mut_ptr(), 2, 4);
        }
        assert_eq!(ch0, [0.0; 4]);
        assert_eq!(ch1, [0.0; 4]);
    }
}
