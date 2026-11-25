//! Gate for the plugin-held [`ProcessWaker`] (CLAP `host.request_process`).
//!
//! The waker escapes into plugin code during `create`, so it must stay
//! harmless whatever the instance's fate:
//! - **Inert until `clap_plugin.init` succeeds.** CLAP forbids host
//!   callbacks during factory creation, and a `create` that panics or
//!   fails returns null without `destroy`, so a gate that was never
//!   opened is the only state a failed creation can leave behind.
//! - **Closed by `destroy`**, which waits for wakes in flight so none
//!   touches the host after the instance is gone.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use moose_core::main_thread::ProcessWaker;

use crate::host_handle::{Any, HostHandle};

const LIVE: usize = 1;
const CLOSED: usize = 2;
/// In-flight wakes are counted above the two flag bits.
const ONE: usize = 4;

#[derive(Default)]
pub(crate) struct WakeGate(AtomicUsize);

impl WakeGate {
    /// Let wakes through (end of a successful `clap_plugin.init`). A closed
    /// gate stays closed.
    pub(crate) fn open(&self) {
        self.0.fetch_or(LIVE, Ordering::AcqRel);
    }

    /// Refuse further wakes and wait for any in flight (`destroy`).
    pub(crate) fn close(&self) {
        self.0.fetch_or(CLOSED, Ordering::AcqRel);
        while self.0.load(Ordering::Acquire) >= ONE {
            std::thread::yield_now();
        }
    }

    /// Run `f` only while the gate is open. Lock- and allocation-free.
    fn with_live(&self, f: impl FnOnce()) {
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            if state & LIVE == 0 || state & CLOSED != 0 {
                return;
            }
            match self.0.compare_exchange_weak(
                state,
                state + ONE,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(now) => state = now,
            }
        }
        f();
        self.0.fetch_sub(ONE, Ordering::Release);
    }
}

/// A waker calling `host.request_process` through `gate`.
pub(crate) fn process_waker(gate: &Arc<WakeGate>, host: HostHandle<Any>) -> ProcessWaker {
    let gate = Arc::clone(gate);
    ProcessWaker::new(move || {
        // SAFETY: the gate is open only between a successful `init` and
        // `destroy`, which closes it (waiting out this call) before the
        // instance releases the host; `request_process` is `[thread-safe]`
        // and cannot unwind (extern "C").
        gate.with_live(|| unsafe { host.request_process() });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn gate_is_inert_until_opened_and_after_close() {
        let gate = WakeGate::default();
        let hits = AtomicU32::new(0);
        let hit = || {
            hits.fetch_add(1, Ordering::Relaxed);
        };
        gate.with_live(hit);
        assert_eq!(hits.load(Ordering::Relaxed), 0, "inert before open");
        gate.open();
        gate.with_live(hit);
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        gate.close();
        gate.with_live(hit);
        gate.open();
        gate.with_live(hit);
        assert_eq!(hits.load(Ordering::Relaxed), 1, "closed stays closed");
    }

    #[test]
    fn close_waits_for_wakes_in_flight() {
        use std::sync::atomic::AtomicBool;
        let gate = Arc::new(WakeGate::default());
        gate.open();
        let inside = Arc::new(AtomicBool::new(false));
        let host_alive = Arc::new(AtomicBool::new(true));
        let waker = {
            let (gate, inside, host_alive) = (
                Arc::clone(&gate),
                Arc::clone(&inside),
                Arc::clone(&host_alive),
            );
            std::thread::spawn(move || {
                gate.with_live(|| {
                    inside.store(true, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(50)); // poll-ok: holds a wake in flight
                    assert!(host_alive.load(Ordering::SeqCst), "host freed mid-wake");
                });
            })
        };
        while !inside.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        gate.close(); // destroy: must not return while the wake runs
        host_alive.store(false, Ordering::SeqCst);
        waker.join().expect("wake completed against a live host");
    }
}
