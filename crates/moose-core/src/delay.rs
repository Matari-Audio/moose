//! The latency and tail a format wrapper reports to its host.
//!
//! Every format asks for these on its main thread (CLAP `latency.get` /
//! `tail.get`, VST3 `getLatencySamples` / `getTailSamples`, AU
//! `kAudioUnitProperty_Latency` / `kAudioUnitProperty_TailTime`) while the
//! processor may be inside `process` on the audio thread. [`ReportedDelay`]
//! keeps each lane's `(reported, seen)` pair packed in a single atomic 64-bit
//! word, so host queries never enter the processor cell and audio-thread
//! observations linearize atomically against main-thread commits.
//!
//! When the processor's latency or tail moves after a block, the audio thread
//! calls [`ReportedDelay::observe_and_signal`] (or
//! [`ReportedDelay::observe_latency_and_signal`] + [`ReportedDelay::publish_tail`]
//! on formats like CLAP 1.2 where `clap.tail.changed` is `[audio-thread]`),
//! raising [`HostSignal::LATENCY`] and/or [`HostSignal::TAIL`] once per new
//! value.
//!
//! How the new value becomes visible depends on the host format's contract:
//!
//! - **CLAP** forbids changing `latency.get()` while active, so CLAP only
//!   publishes latency in `activate` via [`ReportedDelay::publish`]. CLAP 1.2
//!   allows tail updates directly from `[audio-thread]` via
//!   [`ReportedDelay::publish_tail`] + `clap_host_tail.changed`.
//! - **AU** and **VST3** hosts routinely re-read `kAudioUnitProperty_Latency` /
//!   `AUAudioUnit.latency` / `getLatencySamples` immediately inside the
//!   property/KVO/`kLatencyChanged` notification without deactivating first,
//!   so their main-thread drain commits only the lanes covered by the consumed
//!   signals via [`ReportedDelay::commit_latency`] and
//!   [`ReportedDelay::commit_tail`] right before notifying the host.

#[cfg(all(test, feature = "loom"))]
use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(all(test, feature = "loom")))]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::host_queue::{HostQueue, HostSignal};

#[inline]
const fn pack(reported: u32, seen: u32) -> u64 {
    (reported as u64) | ((seen as u64) << 32)
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
const fn unpack_reported(word: u64) -> u32 {
    word as u32
}

#[inline]
const fn unpack_seen(word: u64) -> u32 {
    (word >> 32) as u32
}

/// One independent lock-free audio-to-main delay lane (latency or tail).
///
/// Packs `reported` (low 32 bits) and `seen` (high 32 bits) into one
/// `AtomicU64` so `observe` and `commit` linearize in a single CAS with pure
/// `AcqRel`/`Acquire` ordering — a concurrent observation back to the old
/// reported value can never be overwritten or stranded.
#[derive(Debug, Default)]
struct DelaySlot {
    state: AtomicU64,
}

impl DelaySlot {
    #[cfg(not(all(test, feature = "loom")))]
    const fn new(value: u32) -> Self {
        Self {
            state: AtomicU64::new(pack(value, value)),
        }
    }

    #[cfg(all(test, feature = "loom"))]
    fn new(value: u32) -> Self {
        Self {
            state: AtomicU64::new(pack(value, value)),
        }
    }

    fn publish(&self, value: u32) {
        self.state.store(pack(value, value), Ordering::Release);
    }

    fn publish_rt(&self, value: u32) -> bool {
        let prev = self.state.swap(pack(value, value), Ordering::AcqRel);
        unpack_reported(prev) != value
    }

    fn get(&self) -> u32 {
        unpack_reported(self.state.load(Ordering::Acquire))
    }

    /// Audio thread: record `value` in `seen` and return `true` when `seen`
    /// moved to a value different from `reported`. Allocation-free and
    /// lock-free; uncontended in the common case and retries at most once
    /// against a concurrent main-thread `commit`.
    fn observe(&self, value: u32) -> bool {
        let mut prev = self.state.load(Ordering::Acquire);
        loop {
            let reported = unpack_reported(prev);
            let seen = unpack_seen(prev);
            if seen == value {
                return false;
            }
            let next = pack(reported, value);
            match self
                .state
                .compare_exchange_weak(prev, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return value != reported,
                Err(actual) => prev = actual,
            }
        }
    }

    /// Main thread: atomically promote `seen` into `reported`. Returns `true`
    /// iff `reported` actually changed so the host should be notified.
    fn commit(&self) -> bool {
        let mut prev = self.state.load(Ordering::Acquire);
        loop {
            let reported = unpack_reported(prev);
            let seen = unpack_seen(prev);
            if reported == seen {
                return false;
            }
            let next = pack(seen, seen);
            match self
                .state
                .compare_exchange_weak(prev, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => prev = actual,
            }
        }
    }
}

/// Reported latency and tail, plus the audio thread's last observations.
/// See the [module docs](self).
#[derive(Debug, Default)]
pub struct ReportedDelay {
    latency: DelaySlot,
    tail: DelaySlot,
}

impl ReportedDelay {
    /// Start with `latency` and `tail` reported, e.g. what the plugin says
    /// right after it is created.
    #[cfg(not(all(test, feature = "loom")))]
    #[must_use]
    pub const fn new(latency: u32, tail: u32) -> Self {
        Self {
            latency: DelaySlot::new(latency),
            tail: DelaySlot::new(tail),
        }
    }

    /// Start with `latency` and `tail` reported (loom test builds).
    #[cfg(all(test, feature = "loom"))]
    #[must_use]
    pub fn new(latency: u32, tail: u32) -> Self {
        Self {
            latency: DelaySlot::new(latency),
            tail: DelaySlot::new(tail),
        }
    }

    /// Report new values. Call this from the wrapper's activate / reactivate
    /// path, right after the processor was (re)built, while no `process` runs.
    pub fn publish(&self, latency: u32, tail: u32) {
        self.latency.publish(latency);
        self.tail.publish(tail);
    }

    /// Audio thread, wait-free: immediately publish a new tail value for
    /// formats whose tail notification is audio-thread safe (CLAP 1.2
    /// `clap.tail.changed`). Returns `true` iff the reported tail changed.
    pub fn publish_tail(&self, tail: u32) -> bool {
        self.tail.publish_rt(tail)
    }

    /// Promote the latest audio-observed latency into the host-reported slot.
    ///
    /// Call on the main thread only when [`HostSignal::LATENCY`] was drained.
    /// Returns `true` iff the reported latency changed. Never touches the tail
    /// lane or the processor.
    #[must_use]
    pub fn commit_latency(&self) -> bool {
        self.latency.commit()
    }

    /// Promote the latest audio-observed tail into the host-reported slot.
    ///
    /// Call on the main thread only when [`HostSignal::TAIL`] was drained.
    /// Returns `true` iff the reported tail changed. Never touches the latency
    /// lane or the processor.
    #[must_use]
    pub fn commit_tail(&self) -> bool {
        self.tail.commit()
    }

    /// Promote both observed lanes into the host-reported slots.
    ///
    /// Prefer [`Self::commit_latency`] and [`Self::commit_tail`] keyed on the
    /// exact drained [`HostSignal`] bits so a tail-only drain cannot silently
    /// publish a racing latency observation.
    pub fn commit_observed(&self) {
        let _ = self.latency.commit();
        let _ = self.tail.commit();
    }

    /// The latency in samples the host was told. Any thread; never touches
    /// the processor.
    #[must_use]
    pub fn latency(&self) -> u32 {
        self.latency.get()
    }

    /// The tail in samples the host was told (`u32::MAX` = infinite). Any
    /// thread; never touches the processor.
    #[must_use]
    pub fn tail(&self) -> u32 {
        self.tail.get()
    }

    /// Audio thread, after a block: record what the processor reports now and
    /// return the [`HostSignal`] bits (`LATENCY` and/or `TAIL`) for each value
    /// that moved to a new value differing from what the host was told.
    /// Lock-free and allocation-free.
    pub fn observe_signals(&self, latency: u32, tail: u32) -> HostSignal {
        let mut signals = HostSignal::empty();
        if self.latency.observe(latency) {
            signals |= HostSignal::LATENCY;
        }
        if self.tail.observe(tail) {
            signals |= HostSignal::TAIL;
        }
        signals
    }

    /// Audio thread, after a block: record what the processor reports now.
    /// Returns `true` the first time either value differs from what the host
    /// was told; a value already observed, or one that equals the reported
    /// value again, returns `false`. Lock-free and allocation-free.
    pub fn observe(&self, latency: u32, tail: u32) -> bool {
        !self.observe_signals(latency, tail).is_empty()
    }

    /// Audio thread: observe latency alone and raise [`HostSignal::LATENCY`] on
    /// `queue` when it differs from what the host was told.
    pub fn observe_latency_and_signal(&self, latency: u32, queue: &HostQueue) {
        if self.latency.observe(latency) {
            queue.signal(HostSignal::LATENCY);
        }
    }

    /// [`Self::observe_signals`], raising [`HostSignal::LATENCY`] and/or
    /// [`HostSignal::TAIL`] on `queue` when non-empty. Real-time safe when
    /// `queue`'s wake is real-time safe.
    pub fn observe_and_signal(&self, latency: u32, tail: u32, queue: &HostQueue) {
        let signals = self.observe_signals(latency, tail);
        if !signals.is_empty() {
            queue.signal(signals);
        }
    }

    /// [`Self::observe_signals`], recording the signals on `queue` via
    /// [`HostQueue::signal_no_wake`] (no OS wake syscall on the audio thread).
    /// Returns `true` when at least one signal was recorded.
    pub fn observe_and_signal_no_wake(&self, latency: u32, tail: u32, queue: &HostQueue) -> bool {
        let signals = self.observe_signals(latency, tail);
        if signals.is_empty() {
            return false;
        }
        queue.signal_no_wake(signals);
        true
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;

    #[test]
    fn a_change_signals_once_and_reports_only_after_publish() {
        let delay = ReportedDelay::new(64, 0);
        assert!(!delay.observe(64, 0), "unchanged");
        assert_eq!(delay.observe_signals(128, 0), HostSignal::LATENCY);
        assert!(
            delay.observe_signals(128, 0).is_empty(),
            "already signalled"
        );
        assert_eq!(delay.latency(), 64, "still what the host was told");
        delay.publish(128, 0);
        assert_eq!(delay.latency(), 128);
        assert!(!delay.observe(128, 0), "published value is not a change");
    }

    #[test]
    fn a_tail_change_signals_and_commits_independently_of_latency() {
        let delay = ReportedDelay::new(0, 100);
        assert_eq!(
            delay.observe_signals(64, 200),
            HostSignal::LATENCY | HostSignal::TAIL
        );
        // A tail-only commit must never touch latency.
        assert!(delay.commit_tail());
        assert_eq!(delay.tail(), 200);
        assert_eq!(delay.latency(), 0);
        assert!(delay.observe_signals(64, 200).is_empty());
        assert!(delay.commit_latency());
        assert_eq!(delay.latency(), 64);
    }

    #[test]
    fn going_back_to_the_reported_value_is_quiet() {
        let delay = ReportedDelay::new(10, 0);
        assert!(delay.observe(20, 0));
        assert!(!delay.observe(10, 0), "back to what the host already has");
        assert!(!delay.commit_latency());
        assert_eq!(delay.latency(), 10);
    }

    #[test]
    fn publish_tail_updates_reported_immediately_and_dedups() {
        let delay = ReportedDelay::new(16, 100);
        assert!(!delay.publish_tail(100));
        assert!(delay.publish_tail(512));
        assert_eq!(delay.tail(), 512);
        assert_eq!(delay.latency(), 16);
        assert!(!delay.publish_tail(512));
    }

    #[test]
    fn observe_and_signal_raises_separate_bits_on_the_queue() {
        let queue = HostQueue::new(|| {});
        let delay = ReportedDelay::new(0, 0);
        delay.observe_and_signal(0, 0, &queue);
        assert!(queue.pending().is_empty());
        delay.observe_and_signal(32, 0, &queue);
        assert_eq!(queue.pending(), HostSignal::LATENCY);
        delay.observe_and_signal(32, 512, &queue);
        assert_eq!(queue.pending(), HostSignal::LATENCY | HostSignal::TAIL);
    }

    #[test]
    fn observe_and_signal_no_wake_never_invokes_wake_and_commits_cleanly() {
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let queue = HostQueue::new({
            let wakes = std::sync::Arc::clone(&wakes);
            move || {
                wakes.fetch_add(1, Ordering::Relaxed);
            }
        });
        let delay = ReportedDelay::new(0, 0);
        assert!(!delay.observe_and_signal_no_wake(0, 0, &queue));
        assert!(delay.observe_and_signal_no_wake(64, 1024, &queue));
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        assert_eq!(queue.pending(), HostSignal::LATENCY | HostSignal::TAIL);
        delay.commit_observed();
        assert_eq!(delay.latency(), 64);
        assert_eq!(delay.tail(), 1024);
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::sync::Arc;
    use loom::thread;

    use super::*;

    /// Proves that an audio observation reverting from 20 back to 10 while
    /// `commit_latency` is in flight can never leave `reported` stuck at 20
    /// with no pending signal.
    #[test]
    fn loom_revert_to_reported_racing_commit_never_strands_stale_value() {
        loom::model(|| {
            let delay = Arc::new(ReportedDelay::new(10, 0));
            let queue = Arc::new(HostQueue::new(|| {}));
            delay.observe_latency_and_signal(20, &queue);
            // Main already drained LATENCY and is about to commit.
            let _ = queue.take(HostSignal::LATENCY);

            let audio = thread::spawn({
                let delay = Arc::clone(&delay);
                let queue = Arc::clone(&queue);
                move || {
                    delay.observe_latency_and_signal(10, &queue);
                }
            });

            let _ = delay.commit_latency();
            audio.join().unwrap();

            // If any signal remains pending, a second drain must converge.
            if !queue.take(HostSignal::LATENCY).is_empty() {
                let _ = delay.commit_latency();
            }
            assert_eq!(
                delay.latency(),
                10,
                "reported latency was stranded at stale value 20"
            );
        });
    }

    /// Proves that a tail-only commit racing with a fresh latency observation
    /// never publishes latency silently without leaving `HostSignal::LATENCY`.
    #[test]
    fn loom_tail_commit_never_suppresses_racing_latency_signal() {
        loom::model(|| {
            let delay = Arc::new(ReportedDelay::new(0, 100));
            let queue = Arc::new(HostQueue::new(|| {}));
            assert_eq!(delay.observe_signals(0, 200), HostSignal::TAIL);

            let audio = thread::spawn({
                let delay = Arc::clone(&delay);
                let queue = Arc::clone(&queue);
                move || {
                    delay.observe_and_signal(64, 200, &queue);
                }
            });

            let _ = delay.commit_tail();
            audio.join().unwrap();

            assert_eq!(delay.tail(), 200);
            assert_eq!(delay.latency(), 0);
            assert!(
                queue.pending().contains(HostSignal::LATENCY),
                "latency signal was lost during tail-only commit"
            );
        });
    }
}
