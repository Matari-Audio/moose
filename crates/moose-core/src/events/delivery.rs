//! Receipts for exact events a plugin hands back to the host.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Result of an ACTUAL native output attempt. Core cancellation is deliberately
/// not represented as acceptance and must be handled by the real Core owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExactDeliveryOutcome {
    Accepted,
    NotDelivered,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExactDeliveryError {
    Capacity,
    Overflow,
    Stale,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactDeliveryToken {
    owner: u64,
    index: usize,
    generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactDeliveryReceipt {
    pub token: ExactDeliveryToken,
    pub correlation: u64,
    pub outcome: ExactDeliveryOutcome,
}

const DELIVERY_PHASE_MASK: u64 = 7;
const DELIVERY_RESERVED: u64 = 1;
const DELIVERY_PENDING: u64 = 2;
const DELIVERY_EMITTING: u64 = 3;
const DELIVERY_ACCEPTED: u64 = 4;
const DELIVERY_REFUSED: u64 = 5;
const DELIVERY_CONSUMED: u64 = 8;
const DELIVERY_RELEASED: u64 = 16;
const DELIVERY_SHIFT: u32 = 5;
const DELIVERY_DONE: u64 = DELIVERY_CONSUMED | DELIVERY_RELEASED;

#[derive(Debug)]
struct ExactDeliverySlot {
    tagged: AtomicU64,
    correlation: AtomicU64,
}
#[derive(Debug)]
struct ExactDeliveryStorage {
    owner: u64,
    slots: Vec<ExactDeliverySlot>,
}
static NEXT_EXACT_DELIVERY_BANK: AtomicU64 = AtomicU64::new(1);
/// Persistent bounded instance storage. Construct offaudio and retain permanent
/// host+worker anchors until the worker is joined AND adapter scratch has been
/// destroyed. A carrier owns metadata only, never a Core, Plan, host pointer,
/// Lua value, executable lease, or upload payload. No reset replaces this bank.
#[derive(Clone, Debug)]
pub struct ExactDeliveryBank {
    storage: Arc<ExactDeliveryStorage>,
}
impl ExactDeliveryBank {
    /// # Errors
    /// Returns overflow if the requested storage layout cannot be represented.
    pub fn required_bytes(capacity: usize) -> Result<usize, ExactDeliveryError> {
        let slots = std::alloc::Layout::array::<ExactDeliverySlot>(capacity)
            .map_err(|_| ExactDeliveryError::Overflow)?;
        let (whole, _) = slots
            .extend(std::alloc::Layout::new::<ExactDeliveryStorage>())
            .map_err(|_| ExactDeliveryError::Overflow)?;
        Ok(whole.size())
    }
    /// Requested payload accounting only; caller must additionally price Arc/
    /// allocator layout and all permanent anchors in its whole-product budget.
    ///
    /// # Errors
    /// Returns an error if capacity, budget, allocation, or owner ID is exhausted.
    pub fn new(capacity: usize, payload_budget: usize) -> Result<Self, ExactDeliveryError> {
        if capacity == 0 || Self::required_bytes(capacity)? > payload_budget {
            return Err(ExactDeliveryError::Capacity);
        }
        #[expect(
            clippy::incompatible_msrv,
            reason = "the pinned toolchain has try_update; fetch_update is deprecated"
        )]
        let owner = NEXT_EXACT_DELIVERY_BANK
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |owner| {
                owner.checked_add(1)
            })
            .map_err(|_| ExactDeliveryError::Overflow)?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| ExactDeliveryError::Capacity)?;
        // Price the actual admitted Vec capacity; no shrink/reallocation copy
        // peak is concealed by converting a potentially overallocated Vec.
        if Self::required_bytes(slots.capacity())? > payload_budget {
            return Err(ExactDeliveryError::Capacity);
        }
        for _ in 0..capacity {
            slots.push(ExactDeliverySlot {
                tagged: AtomicU64::new(DELIVERY_DONE),
                correlation: AtomicU64::new(0),
            });
        }
        Ok(Self {
            storage: Arc::new(ExactDeliveryStorage { owner, slots }),
        })
    }
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.storage.slots.len()
    }
    /// Fixed one-pass reservation. A racing peer cannot make this spin. Failed
    /// CAS skips that slot; no unbounded retry and no sequence wrap is allowed.
    ///
    /// # Errors
    /// Returns capacity error if no slot can be reserved.
    pub fn reserve(&self, correlation: u64) -> Result<ExactDeliveryPermit, ExactDeliveryError> {
        for (index, slot) in self.storage.slots.iter().enumerate() {
            let old = slot.tagged.load(Ordering::Acquire);
            if old & DELIVERY_DONE != DELIVERY_DONE {
                continue;
            }
            let Some(generation) = (old >> DELIVERY_SHIFT)
                .checked_add(1)
                .filter(|n| *n <= (u64::MAX >> DELIVERY_SHIFT))
            else {
                continue;
            };
            let next = (generation << DELIVERY_SHIFT) | DELIVERY_RESERVED;
            if slot
                .tagged
                .compare_exchange(old, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                slot.correlation.store(correlation, Ordering::Relaxed);
                return Ok(ExactDeliveryPermit {
                    storage: Arc::clone(&self.storage),
                    token: ExactDeliveryToken {
                        owner: self.storage.owner,
                        index,
                        generation,
                    },
                });
            }
        }
        Err(ExactDeliveryError::Capacity)
    }
    /// Worker housekeeping independent of another positive-frame host process.
    /// Does not consume outcomes until the genuine Core ACK/cancel is complete.
    pub fn visit(&self, mut visit: impl FnMut(ExactDeliveryReceipt)) {
        for (index, slot) in self.storage.slots.iter().enumerate() {
            let tagged = slot.tagged.load(Ordering::Acquire);
            if tagged & DELIVERY_CONSUMED != 0 {
                continue;
            }
            let outcome = match tagged & DELIVERY_PHASE_MASK {
                DELIVERY_ACCEPTED => ExactDeliveryOutcome::Accepted,
                DELIVERY_REFUSED => ExactDeliveryOutcome::NotDelivered,
                _ => continue,
            };
            visit(ExactDeliveryReceipt {
                token: ExactDeliveryToken {
                    owner: self.storage.owner,
                    index,
                    generation: tagged >> DELIVERY_SHIFT,
                },
                correlation: slot.correlation.load(Ordering::Relaxed),
                outcome,
            });
        }
    }
    /// # Errors
    /// Returns stale if the token is invalid or its delivery is not final.
    pub fn consume(&self, token: ExactDeliveryToken) -> Result<(), ExactDeliveryError> {
        self.consume_after(token, |_| {})
    }
    /// One legitimate consumer. Once final, only the original carrier's
    /// RELEASED bit can concurrently change this word. At most one retry is
    /// required; no spin/repeated ACK and no generation/phase weakening.
    fn consume_after(
        &self,
        token: ExactDeliveryToken,
        mut before_cas: impl FnMut(usize),
    ) -> Result<(), ExactDeliveryError> {
        if token.owner != self.storage.owner {
            return Err(ExactDeliveryError::Stale);
        }
        let Some(slot) = self.storage.slots.get(token.index) else {
            return Err(ExactDeliveryError::Stale);
        };
        for attempt in 0..2 {
            let old = slot.tagged.load(Ordering::Acquire);
            if old >> DELIVERY_SHIFT != token.generation
                || old & DELIVERY_CONSUMED != 0
                || !matches!(
                    old & DELIVERY_PHASE_MASK,
                    DELIVERY_ACCEPTED | DELIVERY_REFUSED
                )
            {
                return Err(ExactDeliveryError::Stale);
            }
            before_cas(attempt);
            match slot.tagged.compare_exchange(
                old,
                old | DELIVERY_CONSUMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) if attempt == 0 && actual == (old | DELIVERY_RELEASED) => {}
                Err(_) => return Err(ExactDeliveryError::Stale),
            }
        }
        Err(ExactDeliveryError::Stale)
    }
}
/// Unique owned carrier; intentionally NOT Clone. A failed stage returns it.
#[derive(Debug)]
pub struct ExactDeliveryPermit {
    storage: Arc<ExactDeliveryStorage>,
    token: ExactDeliveryToken,
}
impl ExactDeliveryPermit {
    #[must_use]
    pub fn token(&self) -> ExactDeliveryToken {
        self.token
    }
    fn slot(&self) -> &ExactDeliverySlot {
        &self.storage.slots[self.token.index]
    }
    fn word(&self, phase: u64) -> u64 {
        (self.token.generation << DELIVERY_SHIFT) | phase
    }
    pub(super) fn stage(&self) -> Result<(), ExactDeliveryError> {
        self.slot()
            .tagged
            .compare_exchange(
                self.word(DELIVERY_RESERVED),
                self.word(DELIVERY_PENDING),
                Ordering::Release,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| ExactDeliveryError::Stale)
    }
    pub(super) fn finalize_pending(&self) {
        let _ = self.slot().tagged.compare_exchange(
            self.word(DELIVERY_PENDING),
            self.word(DELIVERY_REFUSED),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
    pub(super) fn begin_native(&self) -> Result<ExactPendingDelivery<'_>, ExactDeliveryError> {
        self.slot()
            .tagged
            .compare_exchange(
                self.word(DELIVERY_PENDING),
                self.word(DELIVERY_EMITTING),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| ExactDeliveryError::Stale)?;
        Ok(ExactPendingDelivery {
            permit: self,
            committed: false,
        })
    }
}
impl Drop for ExactDeliveryPermit {
    fn drop(&mut self) {
        self.finalize_pending();
        // An un-staged reservation has no native debt; resolve its credit.
        let _ = self.slot().tagged.compare_exchange(
            self.word(DELIVERY_RESERVED),
            self.word(DELIVERY_REFUSED) | DELIVERY_CONSUMED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        // This is the unique original carrier. No reuse until worker consumed
        // the terminal outcome as well. Old tagged generations cannot be here:
        // release is the last precondition for any subsequent reservation.
        self.slot()
            .tagged
            .fetch_or(DELIVERY_RELEASED, Ordering::Release);
    }
}
/// Native drain holds this borrowed capability across exactly one real host
/// invocation. Neither a finalizer nor a consumer can change EMITTING. After
/// true, `commit_accepted` is the FIRST action, before sounding-note bookkeeping.
#[derive(Debug)]
pub struct ExactPendingDelivery<'a> {
    permit: &'a ExactDeliveryPermit,
    committed: bool,
}
impl ExactPendingDelivery<'_> {
    pub fn commit_accepted(mut self) {
        self.permit
            .slot()
            .tagged
            .store(self.permit.word(DELIVERY_ACCEPTED), Ordering::Release);
        self.committed = true;
    }
}
impl Drop for ExactPendingDelivery<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.permit
                .slot()
                .tagged
                .store(self.permit.word(DELIVERY_REFUSED), Ordering::Release);
        }
    }
}
#[derive(Debug)]
pub(super) enum ExactDeliveryCarrier {
    Untracked,
    Tracked(ExactDeliveryPermit),
    NonEmittable,
}
impl Clone for ExactDeliveryCarrier {
    fn clone(&self) -> Self {
        match self {
            Self::Untracked => Self::Untracked,
            _ => Self::NonEmittable,
        }
    }
}

#[cfg(test)]
mod exact_delivery_tests {
    use super::ExactDeliverySlot;
    use crate::events::*;
    fn bank(n: usize) -> ExactDeliveryBank {
        ExactDeliveryBank::new(n, ExactDeliveryBank::required_bytes(n).unwrap()).unwrap()
    }
    fn note(id: i32) -> ExactEvent {
        ExactEvent::new(
            0,
            ExactEventBody::Note {
                kind: ExactNoteKind::End,
                address: ExactNoteAddress::from_raw_signed(0, 0, 60, id),
                velocity: 0.,
            },
        )
    }
    fn first(events: &EventList) -> ExactEventRef<'_> {
        match events.lossless_iter().next().unwrap() {
            LosslessEventRef::Exact(event) => event,
            LosslessEventRef::Typed(_) => panic!("exact row"),
        }
    }
    #[test]
    fn original_carrier_release_between_final_load_and_consume_cas_is_not_stale() {
        let bank = bank(1);
        let permit = bank.reserve(71).unwrap();
        let token = permit.token();
        let mut events = EventList::with_capacity(1);
        events.try_push_exact_tracked(note(1), permit).unwrap();
        {
            let pending = first(&events).pending_delivery().unwrap().unwrap();
            pending.commit_accepted();
        }
        let mut phases = 0;
        assert_eq!(
            bank.consume_after(token, |attempt| {
                phases += 1;
                if attempt == 0 {
                    events.clear();
                }
            }),
            Ok(())
        );
        assert_eq!(phases, 2);
        assert!(bank.reserve(72).is_ok());
        assert_eq!(bank.consume(token), Err(ExactDeliveryError::Stale));
    }
    #[test]
    fn checked_layout_limit_plus_one_refuses_before_reservation_or_allocation() {
        let size = core::mem::size_of::<ExactDeliverySlot>();
        let maximum = (isize::MAX as usize) / size;
        // Including storage extends the layout, so even the maximum array can
        // be too large. Both remain checked Result refusals, never Vec panics.
        assert_eq!(
            ExactDeliveryBank::required_bytes(maximum + 1),
            Err(ExactDeliveryError::Overflow)
        );
        assert!(ExactDeliveryBank::new(maximum + 1, usize::MAX).is_err());
        assert_eq!(
            ExactDeliveryBank::required_bytes(usize::MAX),
            Err(ExactDeliveryError::Overflow)
        );
        assert!(ExactDeliveryBank::new(usize::MAX, usize::MAX).is_err());
        let bytes = ExactDeliveryBank::required_bytes(2).unwrap();
        assert!(ExactDeliveryBank::new(2, bytes - 1).is_err());
        assert_eq!(ExactDeliveryBank::new(2, bytes).unwrap().capacity(), 2);
    }
    #[test]
    fn accepted_is_visible_while_host_is_stopped_and_before_carrier_clear() {
        let bank = bank(1);
        let worker = bank.clone();
        let mut events = EventList::with_capacity(1);
        let permit = bank.reserve(71).unwrap();
        let token = permit.token();
        events.try_push_exact_tracked(note(1), permit).unwrap();
        first(&events)
            .pending_delivery()
            .unwrap()
            .unwrap()
            .commit_accepted();
        let mut observed = None;
        worker.visit(|row| observed = Some(row));
        assert_eq!(observed.unwrap().outcome, ExactDeliveryOutcome::Accepted);
        worker.consume(token).unwrap();
        assert!(
            matches!(bank.reserve(72), Err(ExactDeliveryError::Capacity)),
            "carrier is still live"
        );
        events.clear();
        assert!(bank.reserve(72).is_ok());
    }
    #[test]
    fn clone_and_clear_cannot_erase_accepted_or_duplicate_a_permit() {
        let bank = bank(1);
        let mut events = EventList::with_capacity(1);
        let permit = bank.reserve(9).unwrap();
        let token = permit.token();
        events.try_push_exact_tracked(note(2), permit).unwrap();
        let copied = events.clone();
        assert!(!first(&copied).payload().is_emittable());
        assert!(first(&copied).pending_delivery().is_err());
        first(&events)
            .pending_delivery()
            .unwrap()
            .unwrap()
            .commit_accepted();
        events.finalize_pending_delivery();
        events.clear();
        drop(copied);
        let mut outcome = None;
        bank.visit(|row| outcome = Some(row.outcome));
        assert_eq!(outcome, Some(ExactDeliveryOutcome::Accepted));
        assert!(bank.reserve(10).is_err());
        bank.consume(token).unwrap();
        let fresh = bank.reserve(10).unwrap();
        assert_ne!(fresh.token(), token);
        assert_eq!(bank.consume(token), Err(ExactDeliveryError::Stale));
    }
    #[test]
    fn negative_suffix_preserves_accepted_prefix_and_all_credits_are_bounded() {
        let bank = bank(3);
        let mut events = EventList::with_capacity(3);
        for i in 0_u32..3 {
            events
                .try_push_exact_tracked(
                    note(i32::try_from(i).unwrap()),
                    bank.reserve(u64::from(i)).unwrap(),
                )
                .unwrap();
        }
        let mut it = events.lossless_iter();
        let LosslessEventRef::Exact(a) = it.next().unwrap() else {
            panic!("exact")
        };
        a.pending_delivery().unwrap().unwrap().commit_accepted();
        let LosslessEventRef::Exact(b) = it.next().unwrap() else {
            panic!("exact")
        };
        drop(b.pending_delivery().unwrap().unwrap()); // actual false/unwind
        drop(it);
        events.finalize_pending_delivery();
        let mut rows = [None; 3];
        bank.visit(|row| rows[usize::try_from(row.correlation).unwrap()] = Some(row));
        assert_eq!(rows[0].unwrap().outcome, ExactDeliveryOutcome::Accepted);
        for row in &rows[1..] {
            assert_eq!(row.unwrap().outcome, ExactDeliveryOutcome::NotDelivered);
        }
        assert!(bank.reserve(4).is_err());
        events.clear();
        for row in rows.into_iter().flatten() {
            bank.consume(row.token).unwrap();
        }
        assert!(bank.reserve(4).is_ok());
    }
    #[test]
    fn pure_payload_has_no_delivery_capability_and_does_not_settle_pending() {
        let bank = bank(1);
        let mut events = EventList::with_capacity(1);
        events
            .try_push_exact_tracked(note(1), bank.reserve(1).unwrap())
            .unwrap();
        let payload = first(&events).payload();
        assert!(payload.is_emittable());
        assert_eq!(payload.sample_offset(), 0);
        let mut count = 0;
        bank.visit(|_| count += 1);
        assert_eq!(count, 0);
        events.finalize_pending_delivery();
        bank.visit(|row| {
            count += 1;
            assert_eq!(row.outcome, ExactDeliveryOutcome::NotDelivered);
        });
        assert_eq!(count, 1);
    }
    #[test]
    fn native_emission_claim_excludes_negative_finalizer_and_reuse() {
        let bank = bank(1);
        let mut events = EventList::with_capacity(1);
        events
            .try_push_exact_tracked(note(1), bank.reserve(1).unwrap())
            .unwrap();
        let pending = first(&events).pending_delivery().unwrap().unwrap();
        events.finalize_pending_delivery();
        let mut count = 0;
        bank.visit(|_| count += 1);
        assert_eq!(count, 0);
        pending.commit_accepted();
        events.finalize_pending_delivery();
        bank.visit(|row| {
            count += 1;
            assert_eq!(row.outcome, ExactDeliveryOutcome::Accepted);
        });
        assert_eq!(count, 1);
    }
    #[test]
    fn same_slot_and_generation_from_another_bank_is_not_instance_authority() {
        let a = bank(1);
        let b = bank(1);
        let mut ea = EventList::with_capacity(1);
        let mut eb = EventList::with_capacity(1);
        let pa = a.reserve(1).unwrap();
        let ta = pa.token();
        let pb = b.reserve(1).unwrap();
        let tb = pb.token();
        assert_ne!(ta, tb);
        ea.try_push_exact_tracked(note(1), pa).unwrap();
        eb.try_push_exact_tracked(note(2), pb).unwrap();
        first(&ea)
            .pending_delivery()
            .unwrap()
            .unwrap()
            .commit_accepted();
        first(&eb)
            .pending_delivery()
            .unwrap()
            .unwrap()
            .commit_accepted();
        assert_eq!(b.consume(ta), Err(ExactDeliveryError::Stale));
        assert_eq!(a.consume(tb), Err(ExactDeliveryError::Stale));
        a.consume(ta).unwrap();
        b.consume(tb).unwrap();
    }
    #[test]
    fn stage_failure_returns_original_permit_without_losing_reserved_credit() {
        let bank = bank(1);
        let mut events = EventList::with_capacity(0);
        let permit = bank.reserve(3).unwrap();
        let token = permit.token();
        let (_, returned) = events.try_push_exact_tracked(note(1), permit).unwrap_err();
        assert_eq!(returned.token(), token);
        assert!(bank.reserve(4).is_err());
        drop(returned);
        assert!(bank.reserve(4).is_ok());
    }
}
