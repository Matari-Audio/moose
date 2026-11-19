//! VST3 `WaylandSurfaceID`: the editor is a subsurface of the host's surface,
//! pumped by the host's run loop on its UI thread. That window is `!Send` and
//! `Editor: Send`, so the UI thread's table owns it and the editor keeps a key.
//! A key used on any other thread finds nothing.
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::window::wayland::EmbeddedWindow;

thread_local! {
    static WINDOWS: RefCell<HashMap<u64, EmbeddedWindow>> = RefCell::default();
}
static NEXT_KEY: AtomicU64 = AtomicU64::new(0);

/// An embedded editor window owned by the thread that opened it.
pub(super) struct Embedded {
    key: u64,
    fd: RawFd,
}

impl Embedded {
    pub(super) fn adopt(window: EmbeddedWindow) -> Self {
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        let fd = window.fd();
        WINDOWS.with_borrow_mut(|windows| windows.insert(key, window));
        Self { key, fd }
    }

    pub(super) fn fd(&self) -> RawFd {
        self.fd
    }

    /// Runs `f` on the window; `None` off the owner thread.
    pub(super) fn with<R>(&self, f: impl FnOnce(&mut EmbeddedWindow) -> R) -> Option<R> {
        // Taken out of the table while `f` runs: a reentrant `with` from a
        // window callback finds nothing instead of a double borrow.
        let mut window = WINDOWS.with_borrow_mut(|windows| windows.remove(&self.key))?;
        let result = f(&mut window);
        WINDOWS.with_borrow_mut(|windows| windows.insert(self.key, window));
        Some(result)
    }
}

impl Drop for Embedded {
    fn drop(&mut self) {
        // Off the owner thread the window lives until that thread exits.
        let window = WINDOWS
            .try_with(|windows| windows.borrow_mut().remove(&self.key))
            .ok()
            .flatten();
        drop(window);
    }
}
