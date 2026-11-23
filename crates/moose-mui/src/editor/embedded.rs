//! VST3 `WaylandSurfaceID`: the editor is a subsurface of the host's surface,
//! pumped by the host's run loop on its UI thread. That window is `!Send` and
//! `Editor: Send`, so the UI thread's table owns it and the editor keeps a key.
//! A key used on any other thread finds nothing.
//!
//! If an `Embedded` handle is dropped from another thread, the key is queued
//! into the owner thread's retirement inbox instead of silently leaking until
//! thread exit. The next owner-thread entry point (`adopt`, `with`, `drop`, or
//! `reap_retired`) drains that inbox and drops the window on its owner thread.
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use crate::window::wayland::EmbeddedWindow;

struct OwnerTable<W> {
    windows: HashMap<u64, W>,
    retired: Arc<Mutex<Vec<u64>>>,
}

impl<W> Default for OwnerTable<W> {
    fn default() -> Self {
        Self {
            windows: HashMap::new(),
            retired: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl<W> OwnerTable<W> {
    /// Remove any windows whose handles were dropped on another thread.
    ///
    /// Returns the removed windows so the caller drops them *after* releasing
    /// the `RefCell` borrow (preventing re-entrant borrow panics).
    fn take_retired(&mut self) -> Vec<W> {
        let keys: Vec<u64> = {
            let mut retired = self
                .retired
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if retired.is_empty() {
                return Vec::new();
            }
            std::mem::take(&mut *retired)
        };
        keys.into_iter()
            .filter_map(|key| self.windows.remove(&key))
            .collect()
    }
}

thread_local! {
    static WINDOWS: RefCell<OwnerTable<EmbeddedWindow>> = RefCell::default();
}
static NEXT_KEY: AtomicU64 = AtomicU64::new(0);

/// Drop any embedded windows belonging to this thread whose `Embedded` handle
/// was dropped on another thread.
pub(super) fn reap_retired() {
    let stale = WINDOWS
        .try_with(|table| table.borrow_mut().take_retired())
        .unwrap_or_default();
    drop(stale);
}

/// An embedded editor window owned by the thread that opened it.
pub(super) struct Embedded {
    key: u64,
    fd: RawFd,
    owner: ThreadId,
    retired: Arc<Mutex<Vec<u64>>>,
}

impl Embedded {
    pub(super) fn adopt(window: EmbeddedWindow) -> Self {
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        let fd = window.fd();
        let owner = std::thread::current().id();
        let (retired, stale) = WINDOWS.with_borrow_mut(|table| {
            let stale = table.take_retired();
            table.windows.insert(key, window);
            (Arc::clone(&table.retired), stale)
        });
        drop(stale);
        Self {
            key,
            fd,
            owner,
            retired,
        }
    }

    pub(super) fn fd(&self) -> RawFd {
        self.fd
    }

    /// Runs `f` on the window; `None` off the owner thread.
    pub(super) fn with<R>(&self, f: impl FnOnce(&mut EmbeddedWindow) -> R) -> Option<R> {
        // Reap any cross-thread drops before running `f`, and take `self.key`
        // out of the table while `f` runs so a reentrant `with` from a window
        // callback finds nothing instead of a double borrow.
        let (window, stale) = WINDOWS.with_borrow_mut(|table| {
            let stale = table.take_retired();
            let window = table.windows.remove(&self.key);
            (window, stale)
        });
        drop(stale);
        let mut window = window?;
        let result = f(&mut window);
        let stale = WINDOWS.with_borrow_mut(|table| {
            table.windows.insert(self.key, window);
            table.take_retired()
        });
        drop(stale);
        Some(result)
    }
}

impl Drop for Embedded {
    fn drop(&mut self) {
        if std::thread::current().id() == self.owner {
            let (window, stale) = WINDOWS
                .try_with(|table| {
                    let mut table = table.borrow_mut();
                    let window = table.windows.remove(&self.key);
                    let stale = table.take_retired();
                    (window, stale)
                })
                .unwrap_or_default();
            drop(window);
            drop(stale);
        } else {
            let mut retired = self
                .retired
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            retired.push(self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct DropProbe {
        drops: Arc<AtomicUsize>,
        dropped_on: Arc<Mutex<Option<ThreadId>>>,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::AcqRel);
            *self.dropped_on.lock().unwrap() = Some(std::thread::current().id());
        }
    }

    thread_local! {
        static TEST_WINDOWS: RefCell<OwnerTable<DropProbe>> = RefCell::default();
    }

    struct TestEmbedded {
        key: u64,
        owner: ThreadId,
        retired: Arc<Mutex<Vec<u64>>>,
    }

    impl TestEmbedded {
        fn adopt(probe: DropProbe) -> Self {
            let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
            let owner = std::thread::current().id();
            let (retired, stale) = TEST_WINDOWS.with_borrow_mut(|table| {
                let stale = table.take_retired();
                table.windows.insert(key, probe);
                (Arc::clone(&table.retired), stale)
            });
            drop(stale);
            Self {
                key,
                owner,
                retired,
            }
        }

        fn with<R>(&self, f: impl FnOnce(&mut DropProbe) -> R) -> Option<R> {
            let (window, stale) = TEST_WINDOWS.with_borrow_mut(|table| {
                let stale = table.take_retired();
                let window = table.windows.remove(&self.key);
                (window, stale)
            });
            drop(stale);
            let mut window = window?;
            let result = f(&mut window);
            let stale = TEST_WINDOWS.with_borrow_mut(|table| {
                table.windows.insert(self.key, window);
                table.take_retired()
            });
            drop(stale);
            Some(result)
        }

        fn reap_retired() {
            let stale = TEST_WINDOWS
                .try_with(|table| table.borrow_mut().take_retired())
                .unwrap_or_default();
            drop(stale);
        }
    }

    impl Drop for TestEmbedded {
        fn drop(&mut self) {
            if std::thread::current().id() == self.owner {
                let (window, stale) = TEST_WINDOWS
                    .try_with(|table| {
                        let mut table = table.borrow_mut();
                        let window = table.windows.remove(&self.key);
                        let stale = table.take_retired();
                        (window, stale)
                    })
                    .unwrap_or_default();
                drop(window);
                drop(stale);
            } else {
                let mut retired = self
                    .retired
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                retired.push(self.key);
            }
        }
    }

    #[test]
    fn cross_thread_drop_queues_teardown_on_owner_thread() {
        let owner_id = std::thread::current().id();
        let drops = Arc::new(AtomicUsize::new(0));
        let dropped_on = Arc::new(Mutex::new(None));

        let handle = TestEmbedded::adopt(DropProbe {
            drops: Arc::clone(&drops),
            dropped_on: Arc::clone(&dropped_on),
        });

        // Drop the handle from a different thread: the !Send window must not
        // be dropped on the foreign thread, and must be queued for owner reap.
        std::thread::spawn(move || drop(handle)).join().unwrap();
        assert_eq!(drops.load(Ordering::Acquire), 0);

        // Next owner-thread entry reaps the retired window immediately on the
        // owner thread instead of leaking until thread exit.
        TestEmbedded::reap_retired();
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert_eq!(*dropped_on.lock().unwrap(), Some(owner_id));
    }

    #[test]
    fn sibling_with_reaps_cross_thread_dropped_window() {
        let drops_first = Arc::new(AtomicUsize::new(0));
        let drops_second = Arc::new(AtomicUsize::new(0));

        let first = TestEmbedded::adopt(DropProbe {
            drops: Arc::clone(&drops_first),
            dropped_on: Arc::new(Mutex::new(None)),
        });
        let second = TestEmbedded::adopt(DropProbe {
            drops: Arc::clone(&drops_second),
            dropped_on: Arc::new(Mutex::new(None)),
        });

        std::thread::spawn(move || drop(first)).join().unwrap();
        assert_eq!(drops_first.load(Ordering::Acquire), 0);

        // Pumping the live sibling reaps the dropped window cleanly.
        assert!(second.with(|_| ()).is_some());
        assert_eq!(drops_first.load(Ordering::Acquire), 1);
        assert_eq!(drops_second.load(Ordering::Acquire), 0);

        drop(second);
        assert_eq!(drops_second.load(Ordering::Acquire), 1);
    }
}
