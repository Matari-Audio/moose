//! Publish slots for a plugin's custom state, so the host can serialize
//! it without taking the plugin lock. Two lanes cover two size regimes:
//!
//! - **Inline lane** (`publish`): the shell calls `snapshot_into` on the
//!   audio thread after each *dirty* block and copies the bytes here
//!   through `try_lock`, so it never blocks. Right for KB-scale state (a
//!   label, a file path, a few flags). A generation token gates the copy
//!   so an unchanged block pays O(1) (see the shell's `publish_snapshot`).
//!
//! - **Off-thread lane** (`store_arc` via [`SnapshotPublisher`]): a
//!   background thread (e.g. a task-pool handler) serializes large state
//!   and swaps a whole buffer in; the audio thread is never involved. For
//!   MB-scale state (a sampler's audio, big wavetables) that must not be
//!   copied on the audio thread at all. `read` prefers this lane.
//!
//! Both reader entry points ([`SnapshotSlot::read`]) take a blocking
//! `lock`, contending only with each other and with `store_arc`; the
//! audio-thread producer (`publish`) uses `try_lock` and never blocks, so
//! there is no priority inversion on the audio side.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use crate::state::{AudioHeartbeat, StateContext, WAIT_POLL_INTERVAL};

/// Capacity the inline publish buffer is pre-warmed to by
/// [`SnapshotSlot::new`], off the audio thread, so the first per-block
/// publish doesn't allocate. Small state fits without ever growing;
/// larger inline state reallocates once on the first publish, then stays
/// warm. A plugin can raise the pre-warm via `snapshot_prealloc_hint`
/// ([`SnapshotSlot::with_capacity`]). Genuinely large state should use
/// the off-thread lane ([`SnapshotPublisher`]) instead, which never
/// touches this buffer.
pub const SNAPSHOT_PREALLOC: usize = 256;

type TaggedSnapshot = Option<(u64, Arc<Vec<u8>>)>;

/// Shared handle to the published custom-state bytes. Held by the shell
/// (producer, audio thread) and the format wrapper (consumer, host /
/// GUI thread), both cloned from one `Arc`.
pub struct SnapshotSlot {
    /// Inline lane: the audio thread copies serialized bytes here each
    /// dirty block via `try_lock` (see [`Self::publish`]).
    bytes: Mutex<(Option<u64>, Vec<u8>)>,
    slot_id: u64,
    /// Off-thread lane: a background thread swaps a whole pre-serialized
    /// buffer in (see [`Self::store_arc`]), tagged with the [`Self::load_gen`]
    /// observed when it was published. `read` prefers this over `bytes` only
    /// when its generation matches the current `load_gen`, so a state load
    /// never gets overridden by an older background snapshot. The audio thread
    /// never touches this lock.
    swapped: Mutex<TaggedSnapshot>,
    /// Off-thread per-context lane: pre-serialized payloads published for
    /// specific [`StateContext`] values (indexed by `StateContext::to_u8()`),
    /// tagged with `load_gen`. Used during active audio saves so contextual
    /// saves (`Project`, `Preset`, etc.) run strictly on the host/background
    /// thread with zero audio-thread locks or allocations. Never touched on
    /// the audio thread.
    contextual: Mutex<[TaggedSnapshot; StateContext::COUNT]>,
    /// Accepted custom state bytes retained for host-owned pre-activation
    /// decoded loads whose live serializer returns empty (e.g. an unactivated
    /// `Lifecycle` processor). Tagged with `load_gen` so any later state load
    /// invalidates it immediately. Never touched on the audio thread.
    pending_extra: Mutex<Option<(u64, Vec<u8>)>>,
    /// Monotonic state-load generation. Incremented wait-free by
    /// [`Self::mark_state_loaded`] whenever a custom state load lands so older
    /// off-thread publisher snapshots cannot override newer inline state.
    load_gen: AtomicU64,
    /// Set once *either* lane publishes for the first time. Until then a
    /// reader falls back to the locked `save_state()` path.
    supported: AtomicBool,
    /// Fresh-snapshot handshake. A host save that can't own the plugin
    /// (audio is processing) bumps `requested`; the shell serves it at the
    /// end of its next `process` and stores the ticket in `served`.
    requested: AtomicU64,
    served: AtomicU64,
    /// Snapshot ticket associated with the last queued state load applied on
    /// the audio thread, published before `StateLoadQueue::mark_applied` so
    /// the waiting host waits on that original ticket instead of requesting
    /// a second block.
    last_load_state: AtomicUsize,
    last_load_ticket: AtomicU64,
    /// Audio-thread block heartbeat recorded on every `pending_request` poll.
    heartbeat: AudioHeartbeat,
}

impl SnapshotSlot {
    /// A fresh slot with its inline buffer pre-warmed to
    /// [`SNAPSHOT_PREALLOC`]. Nothing is published yet, so [`Self::read`]
    /// returns `None` until the first publish.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Self::with_capacity(SNAPSHOT_PREALLOC)
    }

    /// A fresh slot whose inline buffer is pre-warmed to `cap` bytes.
    /// Used with a plugin's `snapshot_prealloc_hint` so a first inline
    /// publish of up-to-`cap` bytes doesn't reallocate on the audio
    /// thread.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Arc<Self> {
        static NEXT_SLOT: AtomicU64 = AtomicU64::new(1);
        let slot = Self {
            slot_id: NEXT_SLOT.fetch_add(1, Ordering::Relaxed),
            bytes: Mutex::new((None, Vec::with_capacity(cap))),
            swapped: Mutex::new(None),
            contextual: Mutex::new([const { None }; StateContext::COUNT]),
            pending_extra: Mutex::new(None),
            load_gen: AtomicU64::new(0),
            supported: AtomicBool::new(false),
            requested: AtomicU64::new(0),
            served: AtomicU64::new(0),
            last_load_state: AtomicUsize::new(0),
            last_load_ticket: AtomicU64::new(0),
            heartbeat: AudioHeartbeat::new(),
        };
        // Warm locks off the audio thread: some platforms' std `Mutex`
        // (macOS boxes a `pthread_mutex_t`) lazily allocate the OS mutex on
        // first lock, which would otherwise land on the first `publish` from
        // the audio thread. Locking here forces that one-time init at
        // construction instead.
        drop(slot.bytes.lock().unwrap_or_else(PoisonError::into_inner));
        drop(slot.swapped.lock().unwrap_or_else(PoisonError::into_inner));
        drop(
            slot.contextual
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        drop(
            slot.pending_extra
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        Arc::new(slot)
    }

    /// Audio thread: publish the current inline snapshot. The buffer is
    /// **cleared before `write` runs** (its capacity is retained, so a
    /// steady state stays allocation-free), so `write` just fills it and
    /// returns whether a snapshot exists - matching the
    /// `PluginLogicCore::snapshot_into` contract, where a writer that only
    /// `extend`s must not accumulate across blocks. Never blocks - on
    /// lock contention with a reader the publish is skipped and the
    /// previous snapshot stands.
    ///
    /// Returns whether the write **landed** (the lock was acquired and
    /// `write` ran). A `false` return means a reader held the lock and the
    /// publish was skipped, so the caller should not advance its
    /// last-published generation - the block must be retried.
    pub fn publish(&self, write: impl FnOnce(&mut Vec<u8>) -> bool) -> bool {
        let mut guard = match self.bytes.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        guard.1.clear();
        guard.0 = None;
        if write(&mut guard.1) {
            guard.0 = Some(self.load_gen.load(Ordering::Acquire));
            self.supported.store(true, Ordering::Release);
        }
        true
    }

    /// Host thread: replace the inline snapshot, waiting for the lock.
    /// For the host-owned republish (a load applied while the audio thread
    /// is stopped), where skipping on contention would leave pre-load
    /// bytes. Never call this from the audio thread.
    pub fn store_inline(&self, write: impl FnOnce(&mut Vec<u8>)) {
        let mut guard = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
        guard.1.clear();
        write(&mut guard.1);
        guard.0 = (!guard.1.is_empty()).then(|| self.load_gen.load(Ordering::Acquire));
        self.supported.store(true, Ordering::Release);
    }

    /// Host thread: ask the audio thread for a fresh snapshot at the end of
    /// its next block. Returns the ticket to pass to [`Self::wait_served`].
    pub fn request_fresh(&self) -> u64 {
        self.requested.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Audio thread: the newest outstanding request ticket, if any. Also
    /// stamps the audio heartbeat so host saves know blocks are running.
    #[must_use]
    pub fn pending_request(&self) -> Option<u64> {
        self.heartbeat.record_block();
        let requested = self.requested.load(Ordering::Acquire);
        (requested > self.served.load(Ordering::Relaxed)).then_some(requested)
    }

    /// Host thread (on prepare/activate): configure the negotiated block
    /// interval for sleeping-host heartbeat detection.
    pub fn set_block_interval(&self, sample_rate: f64, max_block_size: usize) {
        self.heartbeat
            .set_block_interval(sample_rate, max_block_size);
    }

    /// Audio thread: associate `ticket` with `state_ptr` (from a queued load)
    /// before reporting load completion. Wait-free, zero locks, zero allocs.
    #[inline]
    pub(crate) fn record_load_ticket(&self, state_ptr: usize, ticket: u64) {
        self.last_load_ticket.store(ticket, Ordering::Relaxed);
        self.last_load_state.store(state_ptr, Ordering::Release);
    }

    /// Host thread: return the snapshot ticket requested for `state_ptr` when
    /// it was applied, if no newer load has superseded it.
    #[must_use]
    pub(crate) fn ticket_for_load(&self, state_ptr: usize) -> Option<u64> {
        if self.last_load_state.load(Ordering::Acquire) == state_ptr {
            let ticket = self.last_load_ticket.load(Ordering::Relaxed);
            (ticket > 0).then_some(ticket)
        } else {
            None
        }
    }

    /// Host thread: retain raw custom bytes from an accepted host-owned
    /// decoded load when the live serializer returned empty (e.g. pre-activation
    /// `Lifecycle` slot). Invalidated automatically on the next custom load.
    pub(crate) fn store_pending_extra(&self, bytes: Vec<u8>) {
        let generation = self.load_gen.load(Ordering::Acquire);
        *self
            .pending_extra
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((generation, bytes));
        self.supported.store(true, Ordering::Release);
    }

    /// Host thread: read any pending pre-activation custom bytes for the
    /// current load generation.
    #[must_use]
    pub(crate) fn read_pending_extra(&self) -> Option<Vec<u8>> {
        self.pending_extra
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(generation, _)| *generation == self.load_gen.load(Ordering::Acquire))
            .map(|(_, bytes)| bytes.clone())
    }

    /// Audio or host thread: record that a custom state load landed so any
    /// older off-thread publisher snapshot cannot override the freshly loaded
    /// state. Wait-free, zero locks, zero allocations.
    #[inline]
    pub fn mark_state_loaded(&self) {
        self.load_gen.fetch_add(1, Ordering::AcqRel);
    }

    /// Host thread: true when another thread recently polled
    /// [`Self::pending_request`] from an audio block.
    #[must_use]
    pub fn is_audio_active_on_other_thread(&self) -> bool {
        self.heartbeat.is_active_on_other_thread()
    }

    /// Host thread: mark audio inactive on stop/deactivate so any waiting save
    /// returns promptly without stalling for the handshake timeout.
    pub fn mark_audio_inactive(&self) {
        self.heartbeat.mark_inactive();
    }

    /// Audio thread: serve `ticket` by publishing `write`'s bytes into the
    /// inline lane. On lock contention the request stays outstanding and
    /// the next block retries. Publishes completion with atomics only (zero
    /// waker mutexes or `Thread::unpark` syscalls).
    pub fn serve(&self, ticket: u64, write: impl FnOnce(&mut Vec<u8>)) {
        if self.publish(|buf| {
            write(buf);
            true
        }) {
            self.served.fetch_max(ticket, Ordering::AcqRel);
        }
    }

    /// Host thread: bounded wait timeout scaled to the negotiated audio block
    /// interval (at least `floor`) so large blocks (e.g. 8,192 frames at
    /// 44.1 kHz) do not time out before one block completes.
    #[must_use]
    pub(crate) fn handshake_timeout(&self, floor: Duration) -> Duration {
        self.heartbeat.handshake_timeout(floor)
    }

    /// Host thread: wait up to `timeout` for the audio thread to serve
    /// `ticket`. `false` means no block ran in time (or audio stopped); the
    /// caller falls back to the last published snapshot.
    #[must_use]
    pub fn wait_served(&self, ticket: u64, timeout: Duration) -> bool {
        self.wait_served_poll(ticket, timeout, || {})
    }

    pub(crate) fn wait_served_poll(
        &self,
        ticket: u64,
        timeout: Duration,
        mut on_poll: impl FnMut(),
    ) -> bool {
        let start_gen = self.heartbeat.inactive_gen();
        if self.served.load(Ordering::Acquire) >= ticket {
            return true;
        }
        if timeout.is_zero() || self.heartbeat.was_stopped_since(start_gen) {
            return false;
        }
        let deadline = Instant::now() + timeout;
        let mut spins = 0u8;
        loop {
            on_poll();
            if self.served.load(Ordering::Acquire) >= ticket {
                return true;
            }
            if self.heartbeat.was_stopped_since(start_gen) {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            if spins < 4 {
                spins += 1;
                thread::yield_now();
            } else {
                thread::sleep((deadline - now).min(WAIT_POLL_INTERVAL));
            }
        }
    }

    /// Current state-load generation token. Capture this before starting
    /// off-thread serialization and pass it to [`Self::store_arc_for_generation`]
    /// (or [`SnapshotPublisher::publish_for_generation`]) so a host state load
    /// landing mid-serialization causes the stale snapshot to be rejected.
    #[must_use]
    pub fn generation(&self) -> SnapshotGeneration {
        SnapshotGeneration(self.load_gen.load(Ordering::Acquire), self.slot_id)
    }

    /// Off-thread lane: swap a whole pre-serialized snapshot in. Called
    /// from a background thread (a task-pool handler, a loader worker) -
    /// **never the audio thread** - so the bytes are never copied under a
    /// real-time deadline. Takes read preference in [`Self::read`] while
    /// its load generation matches the current [`Self::load_gen`].
    pub fn store_arc(&self, bytes: Vec<u8>) {
        let generation = self.generation();
        let _ = self.store_arc_for_generation(bytes, generation);
    }

    /// Off-thread lane: publish `bytes` bound to the `generation` captured
    /// when serialization began.
    ///
    /// Returns `true` if stored, or `false` if rejected because a newer host
    /// state load advanced `load_gen` or a newer-generation snapshot is
    /// already stored. Equal-generation updates overwrite normally.
    pub fn store_arc_for_generation(&self, bytes: Vec<u8>, generation: SnapshotGeneration) -> bool {
        let mut guard = self.swapped.lock().unwrap_or_else(PoisonError::into_inner);
        let current = self.load_gen.load(Ordering::Acquire);
        if generation.1 != self.slot_id
            || generation.0 != current
            || guard
                .as_ref()
                .is_some_and(|(stored_gen, _)| *stored_gen > generation.0)
        {
            return false;
        }
        *guard = Some((generation.0, Arc::new(bytes)));
        self.supported.store(true, Ordering::Release);
        true
    }

    /// Off-thread contextual lane: publish `bytes` for a specific
    /// [`StateContext`] (never called on the audio thread). Passing
    /// `StateContext::Unknown` delegates to [`Self::store_arc`]. Publishing
    /// `Vec::new()` explicitly suppresses custom state for `context` (for
    /// example `StateContext::Preset`).
    pub fn store_context_arc(&self, context: StateContext, bytes: Vec<u8>) {
        let generation = self.generation();
        let _ = self.store_context_arc_for_generation(context, bytes, generation);
    }

    /// Off-thread contextual lane: publish `bytes` for `context` bound to
    /// `generation`. Rejected if a newer state load superseded `generation`.
    pub fn store_context_arc_for_generation(
        &self,
        context: StateContext,
        bytes: Vec<u8>,
        generation: SnapshotGeneration,
    ) -> bool {
        if context == StateContext::Unknown {
            return self.store_arc_for_generation(bytes, generation);
        }
        let idx = usize::from(context.to_u8());
        let mut guard = self
            .contextual
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let current = self.load_gen.load(Ordering::Acquire);
        if generation.1 != self.slot_id
            || generation.0 != current
            || guard[idx]
                .as_ref()
                .is_some_and(|(stored_gen, _)| *stored_gen > generation.0)
        {
            return false;
        }
        guard[idx] = Some((generation.0, Arc::new(bytes)));
        self.supported.store(true, Ordering::Release);
        true
    }

    /// Host / GUI thread: read the off-thread contextual payload published for
    /// `context` at the current state-load generation, if any. Never touches
    /// the audio thread.
    #[must_use]
    pub fn read_context_offthread(&self, context: StateContext) -> Option<Vec<u8>> {
        if context == StateContext::Unknown {
            return None;
        }
        let idx = usize::from(context.to_u8());
        self.contextual
            .lock()
            .unwrap_or_else(PoisonError::into_inner)[idx]
            .as_ref()
            .filter(|(generation, _)| *generation == self.load_gen.load(Ordering::Acquire))
            .map(|(_, arc)| (**arc).clone())
    }

    /// Whether the plugin has ever published a snapshot on either lane.
    /// Cheap atomic read (no lock). Once true it stays true: a plugin's
    /// decision to publish snapshots is a static capability, latched on
    /// the first successful publish.
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.supported.load(Ordering::Acquire)
    }

    /// Host / GUI thread: the latest published snapshot, or `None` when
    /// the plugin doesn't publish snapshots (nothing ever written) so
    /// the caller can fall back to the locked `save_state()` path. The
    /// off-thread lane wins over the inline lane only when it belongs to
    /// the current state-load generation; an older publisher snapshot never
    /// overrides a newer state load.
    #[must_use]
    pub fn read(&self) -> Option<Vec<u8>> {
        if !self.supported.load(Ordering::Acquire) {
            return None;
        }
        Some(self.read_current().unwrap_or_default())
    }

    pub(crate) fn read_current(&self) -> Option<Vec<u8>> {
        if let Some(bytes) = self.read_offthread() {
            return Some(bytes);
        }
        let inline = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
        if inline.0 == Some(self.load_gen.load(Ordering::Acquire)) {
            return Some(inline.1.clone());
        }
        drop(inline);
        self.read_pending_extra()
    }

    /// Just the off-thread (publisher) lane's bytes for the current state-load
    /// generation, or `None` when no matching publisher snapshot exists. A
    /// non-realtime save path uses this to pick up a publisher-lane snapshot
    /// the plugin's live `save_state()` can't produce, without letting a
    /// pre-load publisher snapshot override a newer state load.
    #[must_use]
    pub fn read_offthread(&self) -> Option<Vec<u8>> {
        self.swapped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(generation, _)| *generation == self.load_gen.load(Ordering::Acquire))
            .map(|(_, arc)| (**arc).clone())
    }
}

/// Opaque state-load generation token captured before off-thread snapshot
/// serialization begins (via [`SnapshotPublisher::generation`] or
/// [`SnapshotSlot::generation`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotGeneration(u64, u64);

/// Plugin-facing handle for the off-thread snapshot lane. Obtain it from
/// `InitContext::snapshot_publisher()`, stash it in your DSP state, and
/// call [`Self::publish`] (or [`Self::generation`] +
/// [`Self::publish_for_generation`]) from a background-task handler after your
/// custom state changes (a sample load, a wavetable swap). The audio
/// thread never copies the bytes; the host reads whatever was last
/// published. Cloneable and `Send + Sync` so a task handler can hold it.
///
/// Use this **instead of** overriding `snapshot_into` - the two lanes are
/// independent and the off-thread lane takes read preference. It is the
/// recommended path for MB-scale state; `snapshot_into` is for KB-scale.
///
/// **Re-publish after a host load.** The framework can't refresh this
/// lane for you (it never holds your serialized bytes), and the wrapper's
/// post-load republish only drives the inline lane. Publish again from
/// your `state_changed` hook - which runs right after `load_state` - or
/// the host's next save returns the bytes you published *before* the
/// load, silently reverting the loaded preset.
#[derive(Clone)]
pub struct SnapshotPublisher {
    slot: Arc<SnapshotSlot>,
}

impl SnapshotPublisher {
    /// Wrap a slot handle for the plugin to publish through.
    #[must_use]
    pub fn new(slot: &Arc<SnapshotSlot>) -> Self {
        Self {
            slot: Arc::clone(slot),
        }
    }

    /// Capture the current state-load generation before starting off-thread
    /// serialization, then pass the token to [`Self::publish_for_generation`].
    #[must_use]
    pub fn generation(&self) -> SnapshotGeneration {
        self.slot.generation()
    }

    /// Publish a freshly serialized snapshot. Call from a background
    /// thread (never the audio thread) after your custom state changes.
    ///
    /// **Concurrency with host state loads:** `publish` tags `bytes` with the
    /// current `load_gen` at publish time. Producers reading mutable state that
    /// can race with concurrent host state loads **must** capture
    /// [`Self::generation`] *before* reading the source state and call
    /// [`Self::publish_for_generation`] instead so an intervening load is never
    /// overwritten by a stale background snapshot.
    pub fn publish(&self, bytes: Vec<u8>) {
        self.slot.store_arc(bytes);
    }

    /// Publish a snapshot serialized for `generation` (captured via
    /// [`Self::generation`] before serialization began).
    ///
    /// Returns `true` if stored, or `false` if rejected because a host state
    /// load superseded `generation` while serialization was in flight.
    #[must_use]
    pub fn publish_for_generation(&self, generation: SnapshotGeneration, bytes: Vec<u8>) -> bool {
        self.slot.store_arc_for_generation(bytes, generation)
    }

    /// Publish a context-specific snapshot (e.g. `StateContext::Project` vs.
    /// `StateContext::Preset`) off the audio thread so host saves while audio
    /// is active return exact context-specific bytes with zero audio-thread
    /// locks or allocations. Passing `Vec::new()` intentionally suppresses
    /// custom state for `context`.
    pub fn publish_for_context(&self, context: StateContext, bytes: Vec<u8>) {
        self.slot.store_context_arc(context, bytes);
    }

    /// Generation-checked variant of [`Self::publish_for_context`]. Returns
    /// `false` if a newer host state load superseded `generation`.
    #[must_use]
    pub fn publish_for_context_for_generation(
        &self,
        context: StateContext,
        generation: SnapshotGeneration,
        bytes: Vec<u8>,
    ) -> bool {
        self.slot
            .store_context_arc_for_generation(context, bytes, generation)
    }
}

#[cfg(test)]
mod tests {
    use super::{SNAPSHOT_PREALLOC, SnapshotPublisher, SnapshotSlot};

    #[test]
    fn buffer_is_prewarmed_so_first_publish_does_not_allocate() {
        let slot = SnapshotSlot::new();
        // A first publish that fits inside the pre-warmed capacity must
        // not grow the buffer - that is the whole point of warming it
        // off the audio thread.
        let landed = slot.publish(|buf| {
            buf.extend_from_slice(&[0xAB; SNAPSHOT_PREALLOC]);
            true
        });
        assert!(landed);
        let published = slot.read().expect("published");
        assert_eq!(published.len(), SNAPSHOT_PREALLOC);
    }

    #[test]
    fn read_is_none_until_first_publish() {
        let slot = SnapshotSlot::new();
        assert!(slot.read().is_none());
        slot.publish(|buf| {
            buf.extend_from_slice(&[1, 2, 3]);
            true
        });
        assert_eq!(slot.read(), Some(vec![1, 2, 3]));
    }

    #[test]
    fn unsupported_publish_never_marks_supported() {
        let slot = SnapshotSlot::new();
        assert!(slot.publish(|_| false)); // landed, but wrote nothing
        assert!(slot.read().is_none());
    }

    #[test]
    fn is_supported_latches_on_first_true_publish() {
        let slot = SnapshotSlot::new();
        assert!(!slot.is_supported());
        slot.publish(|_| false);
        assert!(!slot.is_supported());
        slot.publish(|buf| {
            buf.push(1);
            true
        });
        assert!(slot.is_supported());
        // A later empty (but true) publish keeps it supported. `buf`
        // arrives cleared, so returning true without writing publishes an
        // empty blob.
        slot.publish(|_| true);
        assert!(slot.is_supported());
        assert_eq!(slot.read(), Some(vec![]));
    }

    #[test]
    fn writer_that_only_appends_does_not_accumulate_across_publishes() {
        // Regression: the framework clears the buffer before each writer
        // runs, so a plugin `snapshot_into` that only `extend`s (per the
        // `PluginLogicCore::snapshot_into` "cleared first" contract) must not
        // append the new snapshot onto the previous one.
        let slot = SnapshotSlot::new();
        slot.publish(|buf| {
            buf.extend_from_slice(&[9; 64]);
            true
        });
        assert_eq!(slot.read(), Some(vec![9; 64]));

        slot.publish(|buf| {
            buf.extend_from_slice(&[7, 7]);
            true
        });
        assert_eq!(
            slot.read(),
            Some(vec![7, 7]),
            "second publish must replace, not append onto, the first"
        );
    }

    #[test]
    fn publish_reports_landed_vs_skipped_on_contention() {
        let slot = SnapshotSlot::new();
        // Hold the inline lock to simulate a reader mid-clone: the publish
        // can't land and reports so, leaving the previous snapshot in place.
        slot.publish(|buf| {
            buf.push(1);
            true
        });
        let held = slot.bytes.lock().unwrap();
        let landed = slot.publish(|buf| {
            buf.clear();
            buf.push(2);
            true
        });
        assert!(!landed, "publish must report a skipped (unlanded) write");
        drop(held);
        assert_eq!(slot.read(), Some(vec![1]), "the prior snapshot stands");
    }

    #[test]
    fn off_thread_lane_takes_read_preference() {
        let slot = SnapshotSlot::new();
        // Inline lane publishes first...
        slot.publish(|buf| {
            buf.extend_from_slice(&[1, 1]);
            true
        });
        assert_eq!(slot.read(), Some(vec![1, 1]));
        // ...then the off-thread lane swaps in a larger buffer, which wins.
        SnapshotPublisher::new(&slot).publish(vec![2; 4096]);
        assert_eq!(slot.read(), Some(vec![2; 4096]));
    }

    #[test]
    fn read_offthread_returns_only_the_publisher_lane() {
        let slot = SnapshotSlot::new();
        // Inline publish alone: no off-thread lane, so read_offthread is None
        // (the LV2 save path then falls back to the live `save_state()`).
        slot.publish(|buf| {
            buf.extend_from_slice(&[1, 1]);
            true
        });
        assert!(slot.read_offthread().is_none());
        // After a publisher-lane store, read_offthread returns those bytes.
        SnapshotPublisher::new(&slot).publish(vec![9; 3]);
        assert_eq!(slot.read_offthread(), Some(vec![9; 3]));
    }

    #[test]
    fn store_arc_latches_supported_without_inline_publish() {
        let slot = SnapshotSlot::new();
        assert!(!slot.is_supported());
        slot.store_arc(vec![5, 5, 5]);
        assert!(slot.is_supported());
        assert_eq!(slot.read(), Some(vec![5, 5, 5]));
    }

    #[test]
    fn request_is_served_once_and_times_out_without_a_block() {
        let slot = SnapshotSlot::new();
        assert!(slot.pending_request().is_none());
        let ticket = slot.request_fresh();
        assert!(!slot.wait_served(ticket, std::time::Duration::from_millis(1)));
        assert_eq!(slot.pending_request(), Some(ticket));
        slot.serve(ticket, |buf| buf.push(4));
        assert!(slot.pending_request().is_none());
        assert!(slot.wait_served(ticket, std::time::Duration::ZERO));
        assert_eq!(slot.read(), Some(vec![4]));
    }

    #[test]
    fn store_inline_replaces_and_marks_supported() {
        let slot = SnapshotSlot::new();
        slot.store_inline(|buf| buf.extend_from_slice(&[1, 2]));
        slot.store_inline(|buf| buf.push(3));
        assert_eq!(slot.read(), Some(vec![3]));
    }

    #[test]
    fn with_capacity_prewarms_the_requested_size() {
        let slot = SnapshotSlot::with_capacity(8192);
        let landed = slot.publish(|buf| {
            buf.extend_from_slice(&[0xCD; 8192]);
            true
        });
        assert!(landed);
        assert_eq!(slot.read().expect("published").len(), 8192);
    }

    #[test]
    fn foreign_generation_cannot_replace_a_snapshot() {
        let a = SnapshotSlot::new();
        let b = SnapshotSlot::new();
        b.store_arc(vec![1]);
        for _ in 0..2 {
            assert!(!b.store_arc_for_generation(vec![2], a.generation()));
            a.mark_state_loaded();
        }
        assert_eq!(b.read(), Some(vec![1]));
        assert!(b.store_arc_for_generation(vec![3], b.generation()));
    }

    #[test]
    fn stale_generation_publish_is_rejected_and_cannot_clobber_newer_generation() {
        let slot = SnapshotSlot::new();
        let publisher = SnapshotPublisher::new(&slot);

        // Background task begins serializing state A at generation 0.
        let gen0 = publisher.generation();

        // A host state load advances the generation to 1 and publishes B.
        slot.mark_state_loaded();
        let gen1 = publisher.generation();
        assert!(gen1 > gen0);
        assert!(publisher.publish_for_generation(gen1, b"GEN1_B".to_vec()));
        assert_eq!(slot.read(), Some(b"GEN1_B".to_vec()));

        // The stale gen-0 serialization finishes after the load and tries to
        // commit: must be rejected and must NOT overwrite GEN1_B.
        assert!(!publisher.publish_for_generation(gen0, b"STALE_A".to_vec()));
        assert_eq!(slot.read(), Some(b"GEN1_B".to_vec()));
        assert_eq!(slot.read_offthread(), Some(b"GEN1_B".to_vec()));

        // Equal-generation updates still succeed cleanly.
        assert!(publisher.publish_for_generation(gen1, b"GEN1_B2".to_vec()));
        assert_eq!(slot.read(), Some(b"GEN1_B2".to_vec()));

        // A stale gen-0 commit is also rejected even if no gen-1 publisher
        // entry has been stored yet (leaving inline state intact).
        slot.mark_state_loaded();
        slot.store_inline(|buf| buf.extend_from_slice(b"INLINE_C"));
        assert!(!publisher.publish_for_generation(gen1, b"STALE_B".to_vec()));
        assert_eq!(slot.read(), Some(b"INLINE_C".to_vec()));
        assert!(slot.read_offthread().is_none());
    }

    #[test]
    fn contextual_offthread_lane_respects_load_gen_and_scales_timeout_for_large_blocks() {
        use crate::state::StateContext;
        use std::time::Duration;

        let slot = SnapshotSlot::new();
        slot.set_block_interval(44_100.0, 8_192);
        let timeout = slot.handshake_timeout(Duration::from_millis(50));
        assert!(
            timeout >= Duration::from_millis(200),
            "8192 frames @ 44.1 kHz needs > 186 ms + margin, got {timeout:?}"
        );

        let publisher = SnapshotPublisher::new(&slot);
        let gen0 = publisher.generation();
        publisher.publish_for_context(StateContext::Project, b"state-a".to_vec());
        publisher.publish_for_context(StateContext::Preset, Vec::new());
        assert_eq!(
            slot.read_context_offthread(StateContext::Project),
            Some(b"state-a".to_vec())
        );
        assert_eq!(
            slot.read_context_offthread(StateContext::Preset),
            Some(Vec::new())
        );
        assert_eq!(slot.read_context_offthread(StateContext::Duplicate), None);

        // Simulating Project B landing invalidates pre-load contextual snapshots.
        slot.mark_state_loaded();
        assert_eq!(slot.read_context_offthread(StateContext::Project), None);
        assert_eq!(slot.read_context_offthread(StateContext::Preset), None);
        assert!(!publisher.publish_for_context_for_generation(
            StateContext::Project,
            gen0,
            b"stale".to_vec()
        ));

        let gen1 = publisher.generation();
        assert!(publisher.publish_for_context_for_generation(
            StateContext::Project,
            gen1,
            b"state-b".to_vec()
        ));
        assert_eq!(
            slot.read_context_offthread(StateContext::Project),
            Some(b"state-b".to_vec())
        );
    }

    #[test]
    fn publish_recovers_after_mutex_poison() {
        let slot = SnapshotSlot::new();
        let poisoner = std::sync::Arc::clone(&slot);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.bytes.lock().unwrap();
            panic!("intentional poison");
        })
        .join();
        assert!(slot.bytes.is_poisoned());

        let landed = slot.publish(|buf| {
            buf.extend_from_slice(b"recovered");
            true
        });
        assert!(landed, "publish must recover from a poisoned inline mutex");
        assert_eq!(slot.read(), Some(b"recovered".to_vec()));
    }
}
