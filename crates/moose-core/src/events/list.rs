//! The per-block event list.

use super::{
    Event, EventBody, ExactDeliveryCarrier, ExactDeliveryPermit, ExactEvent, ExactEventBody,
    ExactEventPayload, ExactEventRef, LosslessEventCursor, LosslessEventRef,
};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_EVENT_LIST_OWNER: AtomicU64 = AtomicU64::new(1);

fn allocate_event_list_owner() -> u64 {
    let mut owner = NEXT_EVENT_LIST_OWNER.load(Ordering::Relaxed);
    loop {
        let next = owner
            .checked_add(1)
            .expect("EventList owner space exhausted");
        match NEXT_EVENT_LIST_OWNER.compare_exchange_weak(
            owner,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(prev) => return prev,
            Err(cur) => owner = cur,
        }
    }
}

/// Default reserved capacity for per-instance `EventList`s held by
/// format wrappers. Sized to cover a heavy MIDI block (note bursts +
/// per-block automation changes) without growing past steady state.
///
/// Plugins can construct a smaller or larger list explicitly via
/// [`EventList::with_capacity`]; this const exists so the format
/// wrappers don't each pick their own magic number.
pub const EVENT_LIST_PREALLOC: usize = 256;

/// Default reserved capacity for the `SysEx` byte pool on
/// per-instance `EventList`s. 128 KiB ≈ 2× the worst-case single
/// payload (one 64 KiB firmware-update-shaped message) with
/// headroom for an interleaved burst of small messages in the
/// same block.
///
/// Sized at construction in [`EventList::with_capacity`]; never
/// re-allocates on the audio thread. A plugin that pushes beyond
/// this gets a [`PushError::PoolFull`] and the message is dropped;
/// truncating or splitting a `SysEx` makes it invalid.
///
/// Must agree with the `MOOSE_SYSEX_POOL_PREALLOC` C macro in the
/// shared shim header: the AU v3 Swift template (which can't import
/// Rust consts) reads the C macro to size its per-render output
/// scratch buffer, and a per-format unit test asserts the two values
/// match.
pub const SYSEX_POOL_PREALLOC: usize = 128 * 1024;

/// Why a push into the bounded [`EventList`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    /// The typed event lane is full.
    EventFull,
    /// The exact event lane is full.
    ExactEventFull,
    /// A companion referred to an exact event owned by another/cleared list.
    UnknownExactEvent,
    /// A host note arrived after the bounded voice tracker became full.
    VoiceTrackerFull,
    /// The `SysEx` byte pool is full. The message wasn't appended.
    /// Callers either drop it, surface it via a meter, or bump the
    /// pool size via [`EventList::with_capacity`] at construction.
    PoolFull,
}

/// Result of the previous attempt to deliver a plugin's output events to the
/// host. [`EventList::clear`] preserves this value, so `process()` can inspect
/// the preceding block before emitting the current one.
///
/// Staging failures remain available through [`EventList::overflow`]; keeping
/// them separate distinguishes framework storage exhaustion from a host queue
/// refusing an otherwise valid event.
///
/// Adapters publish the plugin output lane only, not unrelated GUI-originated
/// control traffic. A staging [`OutputEventStatus::BufferFull`] prevents
/// draining and remains authoritative. Otherwise adapters preflight known
/// [`OutputEventStatus::Invalid`] / [`OutputEventStatus::Unsupported`] blocks
/// before emission; once draining begins, a present carrier's refusal is
/// [`OutputEventStatus::HostQueueFull`]. This ordering keeps independent causes
/// from overwriting one another.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputEventStatus {
    /// Every supported, valid event reached the host.
    #[default]
    Success,
    /// The bounded [`EventList`] could not retain the complete block.
    BufferFull(PushError),
    /// The host's output queue or buffer refused an event.
    HostQueueFull,
    /// The active plugin format cannot represent an emitted event.
    Unsupported,
    /// An emitted event was malformed or outside the current block/port range.
    Invalid,
}

impl core::fmt::Display for PushError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EventFull => f.write_str("event lane is full"),
            Self::ExactEventFull => f.write_str("exact event lane is full"),
            Self::UnknownExactEvent => f.write_str("exact event token is not in this list"),
            Self::VoiceTrackerFull => f.write_str("note voice tracker is full"),
            Self::PoolFull => f.write_str("SysEx byte pool is full"),
        }
    }
}

impl std::error::Error for PushError {}

/// Ordered list of events within a process block.
///
/// `typed` and `exact_events` are the per-block event lanes; `sysex_pool`
/// is the variable-byte arena that typed and exact `SysEx` entries index into.
/// All are pre-allocated by [`EventList::with_capacity`] and reset
/// (length only - backing memory preserved) by [`Self::clear`], so
/// steady-state operation is allocation-free.
#[derive(Debug)]
pub struct EventList {
    typed: Vec<StoredTypedEvent>,
    typed_order: Vec<u32>,
    exact_events: Vec<StoredExactEvent>,
    exact_order: Vec<u32>,
    sysex_pool: Vec<u8>,
    overflow: Option<PushError>,
    output_status: OutputEventStatus,
    token_owner: u64,
    next_sequence: u64,
    typed_sorted: bool,
    last_typed_offset: u32,
    exact_sorted: bool,
    last_exact_offset: u32,
}

#[derive(Clone, Debug)]
pub(super) struct StoredTypedEvent {
    pub(super) event: Event,
    split_before: bool,
    exact_link: EventExactLink,
    pub(super) companion_next: Option<u32>,
    sequence: u64,
}

#[derive(Clone, Debug)]
struct StoredExactEvent {
    event: ExactEvent,
    token: ExactEventToken,
    primary_index: Option<u32>,
    sysex_range: Option<(u32, u32)>,
    first_companion: Option<u32>,
    last_companion: Option<u32>,
    delivery: ExactDeliveryCarrier,
}

/// Stable association token for one exact event and its typed semantic views.
/// Tokens are scoped to an [`EventList`] block and invalidated by `clear()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactEventToken {
    owner: u64,
    exact_index: u32,
    sequence: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EventExactLink {
    #[default]
    None,
    /// The sole typed event allowed as a destination fallback.
    Primary(u32),
    /// Typed semantic fanout visible to plugins but never replayed separately.
    Companion(u32),
}

impl Clone for EventList {
    fn clone(&self) -> Self {
        let token_owner = allocate_event_list_owner();
        let mut exact_events = clone_vec_preserving_capacity(&self.exact_events);
        for stored in &mut exact_events {
            stored.token.owner = token_owner;
        }
        Self {
            typed: clone_vec_preserving_capacity(&self.typed),
            typed_order: clone_vec_preserving_capacity(&self.typed_order),
            exact_events,
            exact_order: clone_vec_preserving_capacity(&self.exact_order),
            sysex_pool: clone_vec_preserving_capacity(&self.sysex_pool),
            overflow: self.overflow,
            output_status: self.output_status,
            token_owner,
            next_sequence: self.next_sequence,
            typed_sorted: self.typed_sorted,
            last_typed_offset: self.last_typed_offset,
            exact_sorted: self.exact_sorted,
            last_exact_offset: self.last_exact_offset,
        }
    }
}

fn clone_vec_preserving_capacity<T: Clone>(source: &Vec<T>) -> Vec<T> {
    let mut cloned = Vec::with_capacity(source.capacity());
    cloned.extend_from_slice(source);
    cloned
}

impl Default for EventList {
    fn default() -> Self {
        Self::with_capacity(EVENT_LIST_PREALLOC)
    }
}

impl EventList {
    /// Construct an `EventList` with backing capacity already reserved.
    ///
    /// Format wrappers build their per-instance event lists at construction
    /// time and reuse them across blocks via `clear()`. [`Self::default`]
    /// reserves [`EVENT_LIST_PREALLOC`]; use this constructor when the host's
    /// maximum event count is known.
    ///
    /// The `SysEx` byte pool is sized to [`SYSEX_POOL_PREALLOC`]
    /// regardless of `capacity` - `capacity` controls both event lanes.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            typed: Vec::with_capacity(capacity),
            typed_order: Vec::with_capacity(capacity),
            exact_events: Vec::with_capacity(capacity),
            exact_order: Vec::with_capacity(capacity),
            sysex_pool: Vec::with_capacity(SYSEX_POOL_PREALLOC),
            overflow: None,
            output_status: OutputEventStatus::Success,
            token_owner: allocate_event_list_owner(),
            next_sequence: 0,
            typed_sorted: true,
            last_typed_offset: 0,
            exact_sorted: true,
            last_exact_offset: 0,
        }
    }

    /// Append an event. Note: `sample_offset` is **not** bounds-checked
    /// against any block size - callers that build event lists per
    /// block must validate `sample_offset < num_samples` themselves
    /// (the audio thread can't recover from an out-of-range offset, so
    /// we treat that as a contract violation rather than panicking).
    pub fn push(&mut self, event: Event) {
        let _ = self.try_push(event);
    }

    /// Append an event that requires a DSP boundary in block-split mode.
    pub fn push_at_boundary(&mut self, event: Event) {
        if self.try_push(event).is_ok()
            && let Some(stored) = self.typed.last_mut()
        {
            stored.split_before = true;
        }
    }

    pub(crate) fn requires_boundary(&self, index: usize) -> bool {
        self.typed[self.typed_order[index] as usize].split_before
    }

    /// Fallible form of [`Self::push`]. No lane grows beyond the capacity
    /// reserved at construction.
    ///
    /// # Errors
    /// [`PushError::EventFull`] when the typed lane is full.
    pub fn try_push(&mut self, event: Event) -> Result<(), PushError> {
        let event_index = self.reserve_event_slot()?;
        let sequence = self.take_sequence();
        self.push_typed_entry(event_index, event, EventExactLink::None, sequence);
        Ok(())
    }

    /// Append an exact-only event which has no typed fallback.
    ///
    /// # Errors
    /// [`PushError::ExactEventFull`] when the exact lane is full.
    pub fn try_push_exact(&mut self, event: ExactEvent) -> Result<(), PushError> {
        self.try_push_exact_token(event).map(|_| ())
    }

    /// Append an exact-only event and return its stable companion token.
    ///
    /// # Errors
    /// [`PushError::ExactEventFull`] when the exact lane is full.
    pub fn try_push_exact_token(
        &mut self,
        event: ExactEvent,
    ) -> Result<ExactEventToken, PushError> {
        let exact_index = self.reserve_exact_slot()?;
        let token = self.take_exact_token(exact_index);
        self.push_exact_entry(
            exact_index,
            StoredExactEvent {
                event,
                token,
                primary_index: None,
                sysex_range: None,
                first_companion: None,
                last_companion: None,
                delivery: ExactDeliveryCarrier::Untracked,
            },
        );
        Ok(token)
    }

    /// Stage one exact event with its uniquely owned native-delivery permit.
    /// All event capacity is checked before the permit becomes Pending. Failure
    /// returns the permit, so its real owner decides cancellation/credit reuse.
    /// Does not allocate and never changes ordinary untracked staging behavior.
    ///
    /// # Errors
    /// Returns the push error and permit if the lane is full or the permit is stale.
    pub fn try_push_exact_tracked(
        &mut self,
        event: ExactEvent,
        permit: ExactDeliveryPermit,
    ) -> Result<ExactEventToken, (PushError, ExactDeliveryPermit)> {
        let exact_index = match self.reserve_exact_slot() {
            Ok(index) => index,
            Err(error) => return Err((error, permit)),
        };
        if permit.stage().is_err() {
            return Err((PushError::UnknownExactEvent, permit));
        }
        let token = self.take_exact_token(exact_index);
        self.push_exact_entry(
            exact_index,
            StoredExactEvent {
                event,
                token,
                primary_index: None,
                sysex_range: None,
                first_companion: None,
                last_companion: None,
                delivery: ExactDeliveryCarrier::Tracked(permit),
            },
        );
        Ok(token)
    }

    /// Actual outer adapter exit guard calls this once for every outcome.
    /// Only still-Pending rows become negative; Accepted is never overwritten.
    /// Clearing/dropping a carrier applies the same pending-only rule.
    pub fn finalize_pending_delivery(&self) {
        for stored in &self.exact_events {
            if let ExactDeliveryCarrier::Tracked(permit) = &stored.delivery {
                permit.finalize_pending();
            }
        }
    }

    /// Copy a `SysEx` payload into the bounded pool and append an exact-only
    /// event which owns that payload independently of any typed companion.
    ///
    /// # Errors
    /// [`PushError::ExactEventFull`] or [`PushError::PoolFull`] when the
    /// corresponding bounded storage rejects the append.
    pub fn try_push_exact_sysex_token(
        &mut self,
        sample_offset: u32,
        data: &[u8],
        mut exact: ExactEvent,
    ) -> Result<ExactEventToken, PushError> {
        if !matches!(exact.body, ExactEventBody::SysEx { .. }) {
            return self.fail(PushError::UnknownExactEvent);
        }
        let exact_index = self.reserve_exact_slot()?;
        let pool_offset = self.sysex_pool.len();
        let Some(pool_end) = pool_offset.checked_add(data.len()) else {
            return self.fail(PushError::PoolFull);
        };
        if pool_end > self.sysex_pool.capacity() {
            return self.fail(PushError::PoolFull);
        }

        exact.sample_offset = sample_offset;
        let token = self.take_exact_token(exact_index);
        self.sysex_pool.extend_from_slice(data);
        #[allow(clippy::cast_possible_truncation)]
        let sysex_range = Some((pool_offset as u32, data.len() as u32));
        self.push_exact_entry(
            exact_index,
            StoredExactEvent {
                event: exact,
                token,
                primary_index: None,
                sysex_range,
                first_companion: None,
                last_companion: None,
                delivery: ExactDeliveryCarrier::Untracked,
            },
        );
        Ok(token)
    }

    /// Attach a typed `SysEx` view to an exact `SysEx` event without copying its
    /// payload a second time. The companion remains suppressed by lossless
    /// replay while typed plugin code can resolve it through this list's pool.
    ///
    /// # Errors
    /// [`PushError::UnknownExactEvent`] when the token is foreign or does not
    /// own an exact `SysEx` payload, or [`PushError::EventFull`] when the typed
    /// lane is full.
    pub fn try_push_exact_sysex_view_companion(
        &mut self,
        token: ExactEventToken,
        sample_offset: u32,
        port: u8,
    ) -> Result<(), PushError> {
        let Some(exact_index) = self.exact_index_for_token(token) else {
            return self.fail(PushError::UnknownExactEvent);
        };
        let Some((pool_offset, len)) = self
            .exact_events
            .get(exact_index as usize)
            .and_then(|stored| stored.sysex_range)
        else {
            return self.fail(PushError::UnknownExactEvent);
        };
        self.try_push_exact_companion(
            token,
            Event::on_port(sample_offset, port, EventBody::SysEx { pool_offset, len }),
        )
    }

    /// Atomically append a typed convenience event and its exact payload.
    /// The exact timestamp is anchored to the typed event so subsequent
    /// offset mutation cannot split the pair.
    ///
    /// # Errors
    /// [`PushError::EventFull`] or [`PushError::ExactEventFull`] when the
    /// corresponding lane is full. Neither lane changes on failure.
    pub fn try_push_with_exact(
        &mut self,
        event: Event,
        exact: ExactEvent,
    ) -> Result<(), PushError> {
        self.try_push_with_exact_token(event, exact).map(|_| ())
    }

    /// Atomically append a primary typed fallback and its exact event, returning
    /// a token which can own additional semantic-only companions.
    ///
    /// # Errors
    /// [`PushError::EventFull`] or [`PushError::ExactEventFull`] when the
    /// corresponding lane is full. Neither lane changes on failure.
    pub fn try_push_with_exact_token(
        &mut self,
        event: Event,
        mut exact: ExactEvent,
    ) -> Result<ExactEventToken, PushError> {
        let event_index = self.reserve_event_slot()?;
        let exact_index = self.reserve_exact_slot()?;

        exact.sample_offset = event.sample_offset;
        let token = self.take_exact_token(exact_index);
        self.push_typed_entry(
            event_index,
            event,
            EventExactLink::Primary(exact_index),
            token.sequence,
        );
        self.push_exact_entry(
            exact_index,
            StoredExactEvent {
                event: exact,
                token,
                primary_index: Some(event_index),
                sysex_range: None,
                first_companion: None,
                last_companion: None,
                delivery: ExactDeliveryCarrier::Untracked,
            },
        );
        Ok(token)
    }

    /// Append a semantic-only typed companion owned by `token`. Companions are
    /// visible through [`Self::iter`] but suppressed by [`Self::lossless_iter`].
    ///
    /// # Errors
    /// [`PushError::UnknownExactEvent`] when the token is not owned by this
    /// list, or [`PushError::EventFull`] when the typed lane is full.
    pub fn try_push_exact_companion(
        &mut self,
        token: ExactEventToken,
        event: Event,
    ) -> Result<(), PushError> {
        let Some(exact_index) = self.exact_index_for_token(token) else {
            return self.fail(PushError::UnknownExactEvent);
        };
        let event_index = self.reserve_event_slot()?;
        let sequence = self.take_sequence();
        self.push_typed_entry(
            event_index,
            event,
            EventExactLink::Companion(exact_index),
            sequence,
        );
        self.link_companion(exact_index, event_index);
        Ok(())
    }

    /// Copy a `SysEx` payload into the pool as a semantic companion of an
    /// existing exact event.
    ///
    /// # Errors
    /// [`PushError::UnknownExactEvent`], [`PushError::EventFull`], or
    /// [`PushError::PoolFull`] when the corresponding bounded storage rejects
    /// the append. No storage changes on failure.
    pub fn try_push_sysex_exact_companion(
        &mut self,
        token: ExactEventToken,
        sample_offset: u32,
        port: u8,
        data: &[u8],
    ) -> Result<(), PushError> {
        let Some(exact_index) = self.exact_index_for_token(token) else {
            return self.fail(PushError::UnknownExactEvent);
        };
        let event_index = self.reserve_event_slot()?;
        let pool_offset = self.sysex_pool.len();
        let Some(pool_end) = pool_offset.checked_add(data.len()) else {
            return self.fail(PushError::PoolFull);
        };
        if pool_end > self.sysex_pool.capacity() {
            return self.fail(PushError::PoolFull);
        }

        self.sysex_pool.extend_from_slice(data);
        #[allow(clippy::cast_possible_truncation)]
        let event = Event::on_port(
            sample_offset,
            port,
            EventBody::SysEx {
                pool_offset: pool_offset as u32,
                len: data.len() as u32,
            },
        );
        let sequence = self.take_sequence();
        self.push_typed_entry(
            event_index,
            event,
            EventExactLink::Companion(exact_index),
            sequence,
        );
        self.link_companion(exact_index, event_index);
        Ok(())
    }

    /// Sort the list by `sample_offset` if it isn't already, keeping
    /// the push order of equal-offset events (a recentre bend must stay
    /// ahead of the note-off it precedes). Hosts require output queues
    /// ordered by time; wrappers call this before draining so a plugin
    /// that pushed block-level events after per-event ones can't hand
    /// the host an unsorted queue; wrappers also sort the merged
    /// *input* stream before processing. Audio-thread safe: already-sorted
    /// blocks return in O(1), and unsorted blocks permute only preallocated
    /// `u32` index lanes via allocation-free `sort_unstable_by_key`. The
    /// globally unique sequence component keeps equal-offset events in push
    /// order despite the unstable sort. Linked exact payloads follow their
    /// fallback while exact-only events are sorted in their own lane. `SysEx`
    /// pool offsets stay valid because the pool's bytes aren't moved.
    pub fn ensure_sorted_by_offset(&mut self) {
        if !self.typed_sorted {
            let typed = &self.typed;
            self.typed_order.sort_unstable_by_key(|&index| {
                let stored = &typed[index as usize];
                (stored.event.sample_offset, stored.sequence)
            });
            self.typed_sorted = true;
            self.last_typed_offset = self
                .typed_order
                .last()
                .map_or(0, |&index| self.typed[index as usize].event.sample_offset);
        }

        if !self.exact_sorted {
            let typed = &self.typed;
            let exact_events = &self.exact_events;
            self.exact_order.sort_unstable_by_key(|&index| {
                let stored = &exact_events[index as usize];
                let sample_offset = stored
                    .primary_index
                    .map_or(stored.event.sample_offset, |primary| {
                        typed[primary as usize].event.sample_offset
                    });
                (sample_offset, stored.token.sequence)
            });
            self.exact_sorted = true;
            self.last_exact_offset = self
                .exact_order
                .last()
                .map_or(0, |&index| self.exact_offset(index as usize));
        }
    }

    /// Append a `SysEx` event whose payload is copied into the pool.
    /// `data` is the inner `SysEx` bytes **without** the leading
    /// `0xF0` / trailing `0xF7` - wrappers strip those at the
    /// boundary.
    ///
    /// Returns [`PushError::PoolFull`] when the pool can't hold
    /// `data.len()` more bytes; the event is *not* appended and the
    /// pool is left unchanged. `SysEx` messages are atomic by spec,
    /// so the caller's choices are drop-and-flag (via a meter) or
    /// fail the host call. Splitting / truncating produces a corrupt
    /// message and is never the right answer.
    ///
    /// # Errors
    /// [`PushError::EventFull`] when the event lane is full, or
    /// [`PushError::PoolFull`] when the byte pool is at capacity.
    pub fn push_sysex(&mut self, sample_offset: u32, data: &[u8]) -> Result<(), PushError> {
        self.push_sysex_on_port(sample_offset, 0, data)
    }

    /// Like [`Self::push_sysex`] but stamps the event with a MIDI
    /// [`Event::port`]. Single-port callers use [`Self::push_sysex`]
    /// (port `0`); a multi-port wrapper preserves the port a `SysEx`
    /// arrived on.
    ///
    /// # Errors
    /// [`PushError::EventFull`] when the event lane is full, or
    /// [`PushError::PoolFull`] when the byte pool is at capacity.
    pub fn push_sysex_on_port(
        &mut self,
        sample_offset: u32,
        port: u8,
        data: &[u8],
    ) -> Result<(), PushError> {
        self.push_sysex_on_port_impl(sample_offset, port, data, None)
            .map(|_| ())
    }

    /// Atomically copy a `SysEx` payload and link its typed event to an exact
    /// payload. This is primarily used when rebasing a lossless event list.
    ///
    /// # Errors
    /// [`PushError::EventFull`], [`PushError::ExactEventFull`], or
    /// [`PushError::PoolFull`] when the corresponding bounded storage is full.
    pub fn try_push_sysex_with_exact_on_port(
        &mut self,
        sample_offset: u32,
        port: u8,
        data: &[u8],
        exact: ExactEvent,
    ) -> Result<(), PushError> {
        self.try_push_sysex_with_exact_on_port_token(sample_offset, port, data, exact)
            .map(|_| ())
    }

    /// Token-returning form of [`Self::try_push_sysex_with_exact_on_port`].
    ///
    /// # Errors
    /// [`PushError::EventFull`], [`PushError::ExactEventFull`], or
    /// [`PushError::PoolFull`] when the corresponding bounded storage is full.
    pub fn try_push_sysex_with_exact_on_port_token(
        &mut self,
        sample_offset: u32,
        port: u8,
        data: &[u8],
        exact: ExactEvent,
    ) -> Result<ExactEventToken, PushError> {
        self.push_sysex_on_port_impl(sample_offset, port, data, Some(exact))?
            .ok_or(PushError::UnknownExactEvent)
    }

    fn push_sysex_on_port_impl(
        &mut self,
        sample_offset: u32,
        port: u8,
        data: &[u8],
        exact: Option<ExactEvent>,
    ) -> Result<Option<ExactEventToken>, PushError> {
        let pool_offset = self.sysex_pool.len();
        let event_index = self.reserve_event_slot()?;
        let exact_index = if exact.is_some() {
            Some(self.reserve_exact_slot()?)
        } else {
            None
        };
        let Some(pool_end) = pool_offset.checked_add(data.len()) else {
            return self.fail(PushError::PoolFull);
        };
        if pool_end > self.sysex_pool.capacity() {
            return self.fail(PushError::PoolFull);
        }
        self.sysex_pool.extend_from_slice(data);
        // `as u32` casts are bounded: pool capacity is sized in the
        // hundreds of KiB at most, and the bounds check above keeps
        // `pool_offset + data.len()` under capacity, which itself
        // fits in `u32` by construction (`SYSEX_POOL_PREALLOC` ==
        // 128 KiB).
        #[allow(clippy::cast_possible_truncation)]
        let event = Event {
            sample_offset,
            port,
            body: EventBody::SysEx {
                pool_offset: pool_offset as u32,
                len: data.len() as u32,
            },
        };
        let sequence = self.take_sequence();
        let token = exact_index.map(|exact_index| ExactEventToken {
            owner: self.token_owner,
            exact_index,
            sequence,
        });
        self.push_typed_entry(
            event_index,
            event,
            exact_index.map_or(EventExactLink::None, EventExactLink::Primary),
            sequence,
        );
        if let (Some(mut exact), Some(exact_index), Some(token)) = (exact, exact_index, token) {
            exact.sample_offset = sample_offset;
            self.push_exact_entry(
                exact_index,
                StoredExactEvent {
                    event: exact,
                    token,
                    primary_index: Some(event_index),
                    sysex_range: None,
                    first_companion: None,
                    last_companion: None,
                    delivery: ExactDeliveryCarrier::Untracked,
                },
            );
        }
        Ok(token)
    }

    /// Resolve a [`EventBody::SysEx`] entry to its payload bytes.
    /// Returns an empty slice for any other variant or an invalid range. The
    /// latter can only come from a manually constructed `EventBody::SysEx`;
    /// fail closed rather than panicking on the audio thread.
    #[must_use]
    pub fn sysex_bytes(&self, body: &EventBody) -> &[u8] {
        self.sysex_bytes_checked(body).unwrap_or(&[])
    }

    /// Checked form of [`Self::sysex_bytes`].
    #[must_use]
    pub fn sysex_bytes_checked(&self, body: &EventBody) -> Option<&[u8]> {
        match body {
            EventBody::SysEx { pool_offset, len } => {
                let start = *pool_offset as usize;
                let end = start.checked_add(*len as usize)?;
                self.sysex_pool.get(start..end)
            }
            _ => None,
        }
    }

    pub fn clear(&mut self) {
        self.typed.clear();
        self.typed_order.clear();
        self.exact_events.clear();
        self.exact_order.clear();
        self.typed_sorted = true;
        self.last_typed_offset = 0;
        self.exact_sorted = true;
        self.last_exact_offset = 0;
        // `Vec::clear` preserves capacity; the pool stays
        // pre-allocated for the next block.
        self.sysex_pool.clear();
        let maximum_block_sequences = self
            .typed
            .capacity()
            .saturating_add(self.exact_events.capacity());
        let maximum_block_sequences = u64::try_from(maximum_block_sequences).unwrap_or(u64::MAX);
        if self
            .next_sequence
            .checked_add(maximum_block_sequences)
            .is_none()
        {
            self.token_owner = allocate_event_list_owner();
            self.next_sequence = 0;
        }
    }

    /// Record a bounded side-channel overflow in this event list. The signal
    /// remains sticky until [`Self::clear_overflow`] is called.
    pub fn record_overflow(&mut self, error: PushError) {
        self.overflow = Some(error);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Event> {
        self.iter_from(0)
    }

    /// Typed events from position `from` on, in order. It slices the order
    /// index, so resuming mid-list is O(1), where `iter().skip(from)` walks
    /// the whole prefix.
    pub fn iter_from(&self, from: usize) -> impl Iterator<Item = &Event> {
        self.typed_order
            .get(from..)
            .unwrap_or_default()
            .iter()
            .filter_map(|&index| self.typed.get(index as usize).map(|stored| &stored.event))
    }

    /// Exact events in stable timestamp order after
    /// [`Self::ensure_sorted_by_offset`]. Linked entries expose their typed
    /// fallback; exact-only entries return `None` from
    /// [`ExactEventRef::fallback`].
    pub fn exact_iter(&self) -> impl Iterator<Item = ExactEventRef<'_>> {
        self.exact_order
            .iter()
            .filter_map(|&index| self.exact_ref(index as usize))
    }

    #[must_use]
    pub fn exact_get(&self, index: usize) -> Option<ExactEventRef<'_>> {
        self.exact_ref(*self.exact_order.get(index)? as usize)
    }

    /// Return the exact payload linked to a typed event index, if any.
    /// Adapters which keep their existing typed loop can use this to suppress
    /// the fallback when they emit the exact payload separately.
    #[must_use]
    pub fn exact_for_event(&self, event_index: usize) -> Option<ExactEventRef<'_>> {
        let storage_index = *self.typed_order.get(event_index)? as usize;
        let exact_index = match self.typed.get(storage_index)?.exact_link {
            EventExactLink::Primary(index) | EventExactLink::Companion(index) => index,
            EventExactLink::None => return None,
        };
        self.exact_ref(exact_index as usize)
    }

    /// Merge exact events with typed events lacking an exact payload without
    /// allocation. Call [`Self::ensure_sorted_by_offset`] first. Ties retain
    /// the original cross-lane insertion order.
    pub fn lossless_iter(&self) -> impl Iterator<Item = LosslessEventRef<'_>> {
        let mut cursor = LosslessEventCursor::default();
        core::iter::from_fn(move || self.lossless_next(&mut cursor))
    }

    /// Pull one item from the merged lossless view without rescanning earlier
    /// entries. The cursor is list-local and should be reset to `Default` when
    /// the owning list is cleared or resorted.
    pub fn lossless_next<'a>(
        &'a self,
        cursor: &mut LosslessEventCursor,
    ) -> Option<LosslessEventRef<'a>> {
        while self
            .typed_order
            .get(cursor.event_order_index)
            .is_some_and(|&index| {
                self.typed
                    .get(index as usize)
                    .is_some_and(|stored| stored.exact_link != EventExactLink::None)
            })
        {
            cursor.event_order_index += 1;
        }

        let typed_entry = self
            .typed_order
            .get(cursor.event_order_index)
            .and_then(|&index| self.typed.get(index as usize));
        let typed_key = typed_entry.map(|stored| (stored.event.sample_offset, stored.sequence));
        let exact_storage_index = self
            .exact_order
            .get(cursor.exact_order_index)
            .map(|&index| index as usize);
        let exact_key = exact_storage_index.and_then(|index| {
            self.exact_events
                .get(index)
                .map(|stored| (self.exact_offset(index), stored.token.sequence))
        });

        match (typed_key, exact_key) {
            (None, None) => None,
            (Some(_), None) => {
                let event = &typed_entry?.event;
                cursor.event_order_index += 1;
                Some(LosslessEventRef::Typed(event))
            }
            (Some(typed), Some(exact)) if typed <= exact => {
                let event = &typed_entry?.event;
                cursor.event_order_index += 1;
                Some(LosslessEventRef::Typed(event))
            }
            (_, Some(_)) => {
                let event = self.exact_ref(exact_storage_index?)?;
                cursor.exact_order_index += 1;
                Some(LosslessEventRef::Exact(event))
            }
        }
    }

    #[must_use]
    pub fn get(&self, index: usize) -> Option<&Event> {
        self.typed
            .get(*self.typed_order.get(index)? as usize)
            .map(|stored| &stored.event)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.typed.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.typed.is_empty() && self.exact_events.is_empty()
    }

    #[must_use]
    pub fn exact_len(&self) -> usize {
        self.exact_events.len()
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.typed.capacity()
    }

    #[must_use]
    pub fn exact_capacity(&self) -> usize {
        self.exact_events.capacity()
    }

    /// Whether both lanes are already in timestamp order, i.e. whether
    /// [`Self::ensure_sorted_by_offset`] would return without sorting.
    /// Appending in non-decreasing `sample_offset` order keeps this true;
    /// format wrappers merge their input streams to stay on that path.
    #[must_use]
    pub fn is_sorted_by_offset(&self) -> bool {
        self.typed_sorted && self.exact_sorted
    }

    /// The most recent capacity failure, including failures ignored by the
    /// source-compatible [`Self::push`] API. This signal remains set across
    /// [`Self::clear`] until explicitly acknowledged.
    #[must_use]
    pub fn overflow(&self) -> Option<PushError> {
        self.overflow
    }

    pub fn clear_overflow(&mut self) {
        self.overflow = None;
    }

    /// Delivery result for the preceding completed output block.
    #[must_use]
    pub fn output_status(&self) -> OutputEventStatus {
        self.output_status
    }

    /// Publish the completed host-delivery result. Format adapters call this
    /// after draining the list; plugin code reads it on the next block.
    #[doc(hidden)]
    pub fn set_output_status(&mut self, status: OutputEventStatus) {
        self.output_status = status;
    }

    /// Add `shift` to typed and exact timestamps appended at or after the
    /// supplied lane cursors. Saturation matches chunked processing's
    /// existing output-offset behaviour and never panics.
    pub fn shift_offsets_from(&mut self, event_from: usize, exact_from: usize, shift: u32) {
        if shift == 0 {
            return;
        }
        let mut shifted_typed = false;
        for stored in self.typed.iter_mut().skip(event_from) {
            stored.event.sample_offset = stored.event.sample_offset.saturating_add(shift);
            shifted_typed = true;
        }
        let mut shifted_exact = false;
        for stored in self.exact_events.iter_mut().skip(exact_from) {
            stored.event.sample_offset = stored.event.sample_offset.saturating_add(shift);
            shifted_exact = true;
        }
        if shifted_typed {
            self.typed_sorted = false;
            self.exact_sorted = false;
        } else if shifted_exact {
            self.exact_sorted = false;
        }
    }

    /// Current `SysEx` pool usage in bytes. Mainly useful in tests
    /// and for plug-in code that wants to surface "headroom
    /// remaining" in an editor.
    #[must_use]
    pub fn sysex_pool_used(&self) -> usize {
        self.sysex_pool.len()
    }

    /// Total `SysEx` pool capacity in bytes. Stable for the life of
    /// the `EventList` (no audio-thread reallocation).
    #[must_use]
    pub fn sysex_pool_capacity(&self) -> usize {
        self.sysex_pool.capacity()
    }

    fn reserve_event_slot(&mut self) -> Result<u32, PushError> {
        if self.typed.len() >= self.typed.capacity()
            || self.typed_order.len() >= self.typed_order.capacity()
        {
            return self.fail(PushError::EventFull);
        }
        u32::try_from(self.typed.len()).map_err(|_| {
            self.record_overflow(PushError::EventFull);
            PushError::EventFull
        })
    }

    fn reserve_exact_slot(&mut self) -> Result<u32, PushError> {
        if self.exact_events.len() >= self.exact_events.capacity()
            || self.exact_order.len() >= self.exact_order.capacity()
        {
            return self.fail(PushError::ExactEventFull);
        }
        u32::try_from(self.exact_events.len()).map_err(|_| {
            self.record_overflow(PushError::ExactEventFull);
            PushError::ExactEventFull
        })
    }

    fn push_typed_entry(
        &mut self,
        event_index: u32,
        event: Event,
        exact_link: EventExactLink,
        sequence: u64,
    ) {
        if !self.typed.is_empty() && event.sample_offset < self.last_typed_offset {
            self.typed_sorted = false;
        }
        if self.typed_sorted {
            self.last_typed_offset = event.sample_offset;
        }
        self.typed.push(StoredTypedEvent {
            event,
            split_before: false,
            exact_link,
            companion_next: None,
            sequence,
        });
        self.typed_order.push(event_index);
    }

    fn push_exact_entry(&mut self, exact_index: u32, stored: StoredExactEvent) {
        let sample_offset = stored
            .primary_index
            .and_then(|index| self.typed.get(index as usize))
            .map_or(stored.event.sample_offset, |entry| {
                entry.event.sample_offset
            });
        if !self.exact_events.is_empty() && sample_offset < self.last_exact_offset {
            self.exact_sorted = false;
        }
        if self.exact_sorted {
            self.last_exact_offset = sample_offset;
        }
        self.exact_events.push(stored);
        self.exact_order.push(exact_index);
    }

    fn fail<T>(&mut self, error: PushError) -> Result<T, PushError> {
        self.record_overflow(error);
        Err(error)
    }

    fn take_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("EventList sequence invariant exhausted");
        sequence
    }

    fn take_exact_token(&mut self, exact_index: u32) -> ExactEventToken {
        ExactEventToken {
            owner: self.token_owner,
            exact_index,
            sequence: self.take_sequence(),
        }
    }

    fn exact_index_for_token(&self, token: ExactEventToken) -> Option<u32> {
        if token.owner != self.token_owner {
            return None;
        }
        self.exact_events
            .get(token.exact_index as usize)
            .filter(|stored| stored.token == token)
            .map(|_| token.exact_index)
    }

    fn link_companion(&mut self, exact_index: u32, event_index: u32) {
        let Some(stored) = self.exact_events.get_mut(exact_index as usize) else {
            return;
        };
        if let Some(last) = stored.last_companion {
            if let Some(entry) = self.typed.get_mut(last as usize) {
                entry.companion_next = Some(event_index);
            }
        } else {
            stored.first_companion = Some(event_index);
        }
        stored.last_companion = Some(event_index);
    }

    fn exact_ref(&self, exact_index: usize) -> Option<ExactEventRef<'_>> {
        let stored = self.exact_events.get(exact_index)?;
        Some(ExactEventRef {
            delivery: &stored.delivery,
            payload: ExactEventPayload {
                emittable: !matches!(stored.delivery, ExactDeliveryCarrier::NonEmittable),
                exact: &stored.event,
                fallback: stored
                    .primary_index
                    .and_then(|index| self.typed.get(index as usize).map(|entry| &entry.event)),
                sysex_range: stored.sysex_range,
                typed: &self.typed,
                first_companion: stored.first_companion,
                sysex_pool: &self.sysex_pool,
            },
        })
    }

    fn exact_offset(&self, exact_index: usize) -> u32 {
        let Some(stored) = self.exact_events.get(exact_index) else {
            return 0;
        };
        stored
            .primary_index
            .and_then(|index| self.typed.get(index as usize))
            .map_or(stored.event.sample_offset, |entry| {
                entry.event.sample_offset
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_sysex_round_trip() {
        let mut list = EventList::with_capacity(8);
        let payload = b"\x7E\x00\x06\x01"; // device-inquiry reply body
        list.push_sysex(42, payload).expect("pool has room");

        assert_eq!(list.len(), 1);
        let event = list.iter().next().expect("one event");
        assert_eq!(event.sample_offset, 42);
        assert!(matches!(event.body, EventBody::SysEx { .. }));
        assert_eq!(list.sysex_bytes(&event.body), payload);
        assert_eq!(list.sysex_pool_used(), payload.len());
    }

    #[test]
    fn push_sysex_two_messages_carve_pool_independently() {
        let mut list = EventList::with_capacity(8);
        let a = b"\x01\x02\x03";
        let b = b"\x04\x05\x06\x07";
        list.push_sysex(0, a).unwrap();
        list.push_sysex(1, b).unwrap();

        let collected: Vec<_> = list.iter().collect();
        assert_eq!(list.sysex_bytes(&collected[0].body), a);
        assert_eq!(list.sysex_bytes(&collected[1].body), b);
        assert_eq!(list.sysex_pool_used(), a.len() + b.len());
    }

    #[test]
    fn push_sysex_pool_full_is_recoverable() {
        // Construct a tiny pool by going through `with_capacity` with a
        // post-hoc shrink - we can't pass a custom pool size today, so
        // exercise the failure path by overflowing the configured 128 KiB.
        let mut list = EventList::with_capacity(8);
        let big = vec![0u8; SYSEX_POOL_PREALLOC];
        list.push_sysex(0, &big)
            .expect("first fill is exactly the pool");
        let err = list.push_sysex(1, b"\x00").unwrap_err();
        assert_eq!(err, PushError::PoolFull);
        // No partial state: the rejected event isn't queued, the pool
        // length is unchanged.
        assert_eq!(list.len(), 1);
        assert_eq!(list.sysex_pool_used(), SYSEX_POOL_PREALLOC);
    }

    #[test]
    fn clear_preserves_pool_capacity() {
        let mut list = EventList::with_capacity(8);
        let cap_before = list.sysex_pool_capacity();
        list.push_sysex(0, b"\x00\x01\x02").unwrap();
        list.clear();
        assert!(list.is_empty());
        assert_eq!(list.sysex_pool_used(), 0);
        // The whole point of pre-allocation: clearing must not free.
        assert_eq!(list.sysex_pool_capacity(), cap_before);
    }

    #[test]
    fn sort_preserves_sysex_offsets() {
        let mut list = EventList::with_capacity(8);
        let early = b"\x10\x11";
        let late = b"\x20\x21\x22";
        list.push_sysex(100, late).unwrap();
        list.push_sysex(0, early).unwrap();
        list.ensure_sorted_by_offset();

        let collected: Vec<_> = list.iter().collect();
        // Sorted: sample_offset=0 comes first, then 100.
        assert_eq!(collected[0].sample_offset, 0);
        assert_eq!(list.sysex_bytes(&collected[0].body), early);
        assert_eq!(collected[1].sample_offset, 100);
        assert_eq!(list.sysex_bytes(&collected[1].body), late);
    }

    #[test]
    fn sysex_bytes_returns_empty_for_non_sysex() {
        let list = EventList::with_capacity(8);
        let body = EventBody::NoteOn {
            group: 0,
            channel: 0,
            note: 60,
            velocity: 100,
        };
        assert_eq!(list.sysex_bytes(&body), []);
    }

    #[test]
    fn event_constructors_set_port() {
        let body = EventBody::NoteOn {
            group: 0,
            channel: 0,
            note: 60,
            velocity: 100,
        };
        assert_eq!(Event::new(10, body).port, 0);
        assert_eq!(Event::on_port(10, 4, body).port, 4);
    }

    #[test]
    fn push_sysex_on_port_stamps_port() {
        let mut list = EventList::with_capacity(8);
        list.push_sysex_on_port(0, 2, b"\x10\x11").unwrap();
        assert_eq!(list.iter().next().unwrap().port, 2);
    }

    #[test]
    fn ensure_sorted_orders_offsets_and_keeps_equal_offset_order() {
        let on = |ch: u8| EventBody::NoteOn {
            group: 0,
            channel: ch,
            note: 60,
            velocity: 100,
        };
        let mut list = EventList::with_capacity(8);
        // Per-event pushes at real offsets, then block-level pushes at
        // the last sample - the shape a vibrato-style emitter produces.
        list.push(Event::new(10, on(0)));
        list.push(Event::new(510, on(1)));
        list.push(Event::new(0, on(2)));
        list.push(Event::new(510, on(3))); // equal offset: must stay after ch 1
        list.ensure_sorted_by_offset();
        let order: Vec<(u32, u8)> = list
            .iter()
            .map(|e| match e.body {
                EventBody::NoteOn { channel, .. } => (e.sample_offset, channel),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(order, vec![(0, 2), (10, 0), (510, 1), (510, 3)]);
    }

    #[test]
    fn ensure_sorted_tracks_order_across_incremental_pushes_and_shifts() {
        let on = |ch: u8| EventBody::NoteOn {
            group: 0,
            channel: ch,
            note: 60,
            velocity: 100,
        };
        let mut list = EventList::with_capacity(8);
        list.push(Event::new(20, on(0)));
        list.push(Event::new(10, on(1)));
        assert!(!list.typed_sorted);
        list.ensure_sorted_by_offset();
        assert!(list.typed_sorted);
        assert_eq!(list.last_typed_offset, 20);

        // Appending monotonically preserves the sorted fast-path.
        list.push(Event::new(20, on(2)));
        list.push(Event::new(30, on(3)));
        assert!(list.typed_sorted);
        list.ensure_sorted_by_offset();
        let offsets: Vec<(u32, u8)> = list
            .iter()
            .map(|e| match e.body {
                EventBody::NoteOn { channel, .. } => (e.sample_offset, channel),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(offsets, vec![(10, 1), (20, 0), (20, 2), (30, 3)]);

        // Appending out of order invalidates the flag and re-sorts cleanly.
        list.push(Event::new(15, on(4)));
        assert!(!list.typed_sorted);
        list.ensure_sorted_by_offset();
        assert!(list.typed_sorted);
        let offsets: Vec<(u32, u8)> = list
            .iter()
            .map(|e| match e.body {
                EventBody::NoteOn { channel, .. } => (e.sample_offset, channel),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(offsets, vec![(10, 1), (15, 4), (20, 0), (20, 2), (30, 3)]);

        // Shifting a suffix invalidates sortedness when an earlier entry
        // moves past a later one.
        let mut shifted = EventList::with_capacity(8);
        shifted.push(Event::new(50, on(0)));
        let mark = shifted.len();
        shifted.push(Event::new(10, on(1)));
        shifted.shift_offsets_from(mark, 0, 100);
        assert!(!shifted.typed_sorted);
        shifted.ensure_sorted_by_offset();
        let shifted_offsets: Vec<(u32, u8)> = shifted
            .iter()
            .map(|e| match e.body {
                EventBody::NoteOn { channel, .. } => (e.sample_offset, channel),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(shifted_offsets, vec![(50, 0), (110, 1)]);
    }

    #[test]
    fn interleaved_typed_exact_and_param_pushes_stay_on_the_fast_path() {
        use super::super::{ExactEventBody, ExactNoteAddress, ExactNoteKind};

        let note = |offset: u32| {
            (
                Event::new(
                    offset,
                    EventBody::NoteOn {
                        group: 0,
                        channel: 0,
                        note: 60,
                        velocity: 100,
                    },
                ),
                ExactEvent::new(
                    offset,
                    ExactEventBody::Note {
                        kind: ExactNoteKind::On,
                        address: ExactNoteAddress::default(),
                        velocity: 0.75,
                    },
                ),
            )
        };
        let param =
            |offset: u32, id: u32| Event::new(offset, EventBody::ParamChange { id, value: 0.5 });

        // The order a merged VST3 block arrives in: notes (typed + exact)
        // and parameter points by offset, ties keeping push order.
        let mut list = EventList::with_capacity(16);
        let (typed, exact) = note(0);
        list.try_push_with_exact(typed, exact).unwrap();
        list.try_push(param(0, 1)).unwrap();
        list.try_push(param(5, 0)).unwrap();
        let (typed, exact) = note(20);
        list.try_push_with_exact(typed, exact).unwrap();
        list.try_push(param(20, 0)).unwrap();
        list.try_push(param(20, 1)).unwrap();
        list.try_push_exact(note(30).1).unwrap();
        list.try_push(param(63, 1)).unwrap();
        assert!(list.is_sorted_by_offset());

        let key = |e: &Event| (e.sample_offset, e.body);
        let before: Vec<(u32, EventBody)> = list.iter().map(key).collect();
        list.ensure_sorted_by_offset();
        let after: Vec<(u32, EventBody)> = list.iter().map(key).collect();
        assert_eq!(before, after, "the fast path must not reorder anything");

        // One out-of-order push leaves the fast path; sorting restores it.
        list.try_push(param(10, 0)).unwrap();
        assert!(!list.is_sorted_by_offset());
        list.ensure_sorted_by_offset();
        assert!(list.is_sorted_by_offset());
        let offsets: Vec<u32> = list.iter().map(|e| e.sample_offset).collect();
        assert_eq!(offsets, vec![0, 0, 5, 10, 20, 20, 20, 63]);
    }

    #[test]
    fn exact_companions_and_lossless_iter_survive_sorting() {
        use super::super::{ExactEventBody, ExactNoteAddress, ExactNoteKind};

        let mut list = EventList::with_capacity(8);
        let exact_late = ExactEvent::new(
            40,
            ExactEventBody::Note {
                kind: ExactNoteKind::On,
                address: ExactNoteAddress::default(),
                velocity: 0.75,
            },
        );
        let tok = list
            .try_push_with_exact_token(
                Event::new(
                    40,
                    EventBody::NoteOn {
                        group: 0,
                        channel: 1,
                        note: 60,
                        velocity: 96,
                    },
                ),
                exact_late,
            )
            .unwrap();
        list.try_push_exact_companion(
            tok,
            Event::new(
                40,
                EventBody::ControlChange {
                    group: 0,
                    channel: 1,
                    cc: 74,
                    value: 64,
                },
            ),
        )
        .unwrap();
        list.push(Event::new(
            10,
            EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 64,
                velocity: 100,
            },
        ));
        list.ensure_sorted_by_offset();

        // Typed iter sees all 3 events sorted by offset.
        assert_eq!(list.len(), 3);
        let typed_offsets: Vec<u32> = list.iter().map(|e| e.sample_offset).collect();
        assert_eq!(typed_offsets, vec![10, 40, 40]);

        // Lossless iter suppresses primary fallback and companion, yielding
        // standalone typed (10) then exact (40) with its fallback + companion.
        let lossless: Vec<_> = list.lossless_iter().collect();
        assert_eq!(lossless.len(), 2);
        assert!(matches!(lossless[0], LosslessEventRef::Typed(e) if e.sample_offset == 10));
        let LosslessEventRef::Exact(exact_ref) = lossless[1] else {
            panic!("expected exact event");
        };
        assert_eq!(exact_ref.sample_offset(), 40);
        assert!(exact_ref.fallback().is_some());
        assert_eq!(exact_ref.companions().count(), 1);

        // `exact_for_event` resolves sorted indices 1 and 2 to the exact event,
        // and index 0 (standalone) to None.
        assert!(list.exact_for_event(0).is_none());
        assert!(list.exact_for_event(1).is_some());
        assert!(list.exact_for_event(2).is_some());
    }
}
