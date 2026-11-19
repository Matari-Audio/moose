//! Shared transport slot: audio-thread writer → editor-thread reader.
//!
//! Each format wrapper owns a [`TransportSlot`] and writes it at the
//! top of every process block. The editor closure on
//! [`crate::editor::PluginContext`] reads from the same slot, giving
//! UI code access to host tempo / play state / beat position without
//! a format-specific callback.
//!
//! The implementation is a single-writer seqlock over atomic words: the
//! audio thread's write path takes no locks and always lands; UI readers
//! retry on collision. Every field is copied through an `AtomicU64`, so a
//! read racing a write is a well-defined atomic access whose result the
//! sequence check discards, not a data race. A mutex-based design would
//! either drop audio-thread writes on contention (`try_lock`) or
//! introduce audio-thread blocking, both of which would let the
//! visualizer drift from real transport state.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering, fence};

use crate::events::TransportInfo;

/// Words a [`TransportInfo`] is packed into; see [`encode`].
const WORDS: usize = 8;

/// Single-writer / multi-reader transport slot. Held by format
/// wrappers; exposed to editors via `PluginContext::transport`.
///
/// The audio thread calls [`TransportSlot::write`] each block; readers
/// (UI thread, worker threads) call [`TransportSlot::read`].
///
/// The seq counter is 0 before any write, then alternates odd ("write
/// in progress") / even ("write done") as `write` runs. `read` reads
/// the counter, copies the words, re-reads the counter, and retries if
/// either snapshot landed on a write-in-progress or the two reads
/// disagree.
pub struct TransportSlot {
    /// Sequence counter. 0 = uninitialized; even, non-zero = quiescent
    /// after Nth write; odd = writer mid-update.
    seq: AtomicU64,
    /// The last written transport, packed by [`encode`].
    words: [AtomicU64; WORDS],
}

impl TransportSlot {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            seq: AtomicU64::new(0),
            words: std::array::from_fn(|_| AtomicU64::new(0)),
        })
    }

    /// Realtime-safe write. Called on the audio thread at the top of
    /// each process block. Wait-free - never blocks, never drops.
    ///
    /// Single-writer: this assumes only one thread (the host's audio
    /// callback) ever calls `write` on a given slot. Format wrappers
    /// uphold this by giving each plugin instance its own slot.
    pub fn write(&self, info: &TransportInfo) {
        let s = self.seq.load(Ordering::Relaxed);
        // Flip to odd ("write in progress"). The release fence orders this
        // store before the word stores, so a reader that sees any new word
        // also sees the odd (or a later) seq and retries.
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (word, value) in self.words.iter().zip(encode(info)) {
            word.store(value, Ordering::Relaxed);
        }
        // Publish: pairs with `read`'s Acquire load of the even value.
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Read the most recently-reported transport info, or `None` if
    /// no host block has reported one yet.
    ///
    /// Bounded retry: each iteration is an Acquire-ordered counter
    /// load and an eight-word copy. A writer scheduled out mid-update
    /// could make the reader spin, so this gives up after 8 attempts
    /// and returns `None`; the editor reads again next frame.
    pub fn read(&self) -> Option<TransportInfo> {
        for _ in 0..8 {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 == 0 {
                return None;
            }
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let words: [u64; WORDS] =
                std::array::from_fn(|i| self.words[i].load(Ordering::Relaxed));
            // Keep the word loads above from sinking below the second seq
            // load, so a write that started during the copy changes s2.
            fence(Ordering::Acquire);
            let s2 = self.seq.load(Ordering::Relaxed);
            if s1 == s2 {
                return Some(decode(words));
            }
        }
        None
    }
}

/// Pack a transport into words. The exhaustive destructure makes a new
/// `TransportInfo` field a compile error here until it is packed too.
fn encode(info: &TransportInfo) -> [u64; WORDS] {
    let TransportInfo {
        playing,
        recording,
        tempo,
        time_sig_num,
        time_sig_den,
        position_samples,
        position_seconds,
        position_beats,
        bar_start_beats,
        loop_active,
        loop_start_beats,
        loop_end_beats,
    } = *info;
    let flags = u64::from(playing)
        | (u64::from(recording) << 1)
        | (u64::from(loop_active) << 2)
        | (u64::from(time_sig_num) << 8)
        | (u64::from(time_sig_den) << 16);
    [
        flags,
        tempo.to_bits(),
        u64::from_ne_bytes(position_samples.to_ne_bytes()),
        position_seconds.to_bits(),
        position_beats.to_bits(),
        bar_start_beats.to_bits(),
        loop_start_beats.to_bits(),
        loop_end_beats.to_bits(),
    ]
}

/// Inverse of [`encode`].
fn decode(words: [u64; WORDS]) -> TransportInfo {
    let [flags, tempo, samples, seconds, beats, bar, loop_start, loop_end] = words;
    let byte = |shift: u32| (flags >> shift).to_le_bytes()[0];
    TransportInfo {
        playing: flags & 1 != 0,
        recording: flags & 2 != 0,
        loop_active: flags & 4 != 0,
        time_sig_num: byte(8),
        time_sig_den: byte(16),
        tempo: f64::from_bits(tempo),
        position_samples: i64::from_ne_bytes(samples.to_ne_bytes()),
        position_seconds: f64::from_bits(seconds),
        position_beats: f64::from_bits(beats),
        bar_start_beats: f64::from_bits(bar),
        loop_start_beats: f64::from_bits(loop_start),
        loop_end_beats: f64::from_bits(loop_end),
    }
}

#[cfg(test)]
mod tests {
    use super::TransportSlot;
    use crate::events::TransportInfo;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Every field survives the word packing, including negative
    /// positions and the time signature bytes.
    #[test]
    fn encode_round_trips_every_field() {
        let info = TransportInfo {
            playing: true,
            recording: false,
            tempo: 133.25,
            time_sig_num: 7,
            time_sig_den: 8,
            position_samples: -4_096,
            position_seconds: -0.5,
            position_beats: 12.75,
            bar_start_beats: 12.0,
            loop_active: true,
            loop_start_beats: 8.0,
            loop_end_beats: 16.0,
        };
        assert_eq!(super::decode(super::encode(&info)), info);
        let slot = TransportSlot::new();
        assert_eq!(slot.read(), None, "nothing written yet");
        slot.write(&info);
        assert_eq!(slot.read(), Some(info));
    }

    /// Encode one counter into every numeric/bool field, so a whole
    /// (untorn) read has all fields agreeing on the same `n`. A torn read
    /// mixes fields from two writes and fails the check. This is a soak
    /// test, not a proof: on a weak-memory host (`AArch64`) the missing
    /// seqlock fences let torn reads validate, and enough iterations
    /// surface one; on `x86` it mainly guards the retry logic.
    ///
    /// Skipped under Miri: its ~2M iterations across four threads are
    /// hour-scale in the interpreter, and Miri's deterministic scheduler
    /// won't reproduce the hardware weak-memory torn read this soaks for.
    #[allow(clippy::float_cmp, clippy::cast_precision_loss)]
    #[cfg_attr(
        miri,
        ignore = "concurrency soak - too slow under Miri, no weak-memory repro"
    )]
    #[test]
    fn concurrent_reads_never_observe_a_torn_write() {
        let slot = TransportSlot::new();
        let stop = Arc::new(AtomicBool::new(false));

        let writer = {
            let slot = Arc::clone(&slot);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for n in 1i64..2_000_000 {
                    let f = n as f64;
                    slot.write(&TransportInfo {
                        playing: n % 2 == 0,
                        recording: n % 2 == 1,
                        tempo: f,
                        time_sig_num: 0,
                        time_sig_den: 0,
                        position_samples: n,
                        position_seconds: f,
                        position_beats: f,
                        bar_start_beats: f,
                        loop_active: n % 2 == 0,
                        loop_start_beats: f,
                        loop_end_beats: f,
                    });
                }
                stop.store(true, Ordering::Relaxed);
            })
        };

        let readers: Vec<_> = (0..3)
            .map(|_| {
                let slot = Arc::clone(&slot);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if let Some(info) = slot.read() {
                            let n = info.position_samples;
                            let f = n as f64;
                            assert_eq!(info.tempo, f, "torn read: tempo");
                            assert_eq!(info.position_seconds, f, "torn read: seconds");
                            assert_eq!(info.position_beats, f, "torn read: beats");
                            assert_eq!(info.bar_start_beats, f, "torn read: bar");
                            assert_eq!(info.loop_start_beats, f, "torn read: loop start");
                            assert_eq!(info.loop_end_beats, f, "torn read: loop end");
                            assert_eq!(info.playing, n % 2 == 0, "torn read: playing");
                            assert_eq!(info.recording, n % 2 == 1, "torn read: recording");
                        }
                    }
                })
            })
            .collect();

        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }
    }
}
