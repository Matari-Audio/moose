//! Typed thread ownership.
//!
//! A plugin instance has three parts: the shared parameters (`Params`, any
//! thread, atomics only), the main-thread part ([`MainThreadHooks`], the
//! host's main/UI thread only) and the processor (the DSP state, audio
//! thread). The tokens below make "only on this thread" a type: a format
//! wrapper mints [`MainThread`] inside the host callbacks its format runs on
//! the main thread and [`AudioThread`] inside `process`, and APIs that must
//! not run anywhere else take the token. Both are `!Send`, so a token can't
//! leave the callback's thread.
//!
//! [`MainThreadHooks`]: crate::main_thread::MainThreadHooks

use std::cell::{Ref, RefCell, RefMut};
use std::marker::PhantomData;

/// Proof that the caller runs on the host's main (UI) thread.
#[derive(Debug)]
pub struct MainThread {
    _not_send: PhantomData<*const ()>,
}

impl MainThread {
    /// # Safety
    ///
    /// The caller runs on the host's main thread: inside a callback that the
    /// plug-in format specifies for that thread (CLAP `[main-thread]`, VST3
    /// edit-controller / UI-thread calls, AU property calls on the main
    /// queue), or on the standalone's UI thread. Every [`MainThreadCell`]
    /// relies on all tokens coming from that one thread.
    #[must_use]
    pub unsafe fn assume() -> Self {
        Self {
            _not_send: PhantomData,
        }
    }
}

/// Proof that the caller runs on the host's audio thread, inside `process`.
#[derive(Debug)]
pub struct AudioThread {
    _not_send: PhantomData<*const ()>,
}

impl AudioThread {
    /// # Safety
    ///
    /// The caller runs inside the host's process callback (CLAP `[audio-thread]`,
    /// VST3 `IAudioProcessor::process`, AU render).
    #[must_use]
    pub unsafe fn assume() -> Self {
        Self {
            _not_send: PhantomData,
        }
    }
}

/// Data that only the main thread touches. It can sit in a wrapper struct
/// shared with the audio thread: every access needs a [`MainThread`] token.
pub struct MainThreadCell<T> {
    value: RefCell<T>,
}

// SAFETY: the `RefCell` is reached only through `borrow` / `borrow_mut`,
// which take a `&MainThread`. Tokens exist only on the host's main thread
// (`MainThread::assume`'s contract) and are `!Send`, so all accesses happen
// on that single thread, ordered by the host's own callback sequencing.
// `T: Send` because the cell may be created and dropped on other threads.
unsafe impl<T: Send> Sync for MainThreadCell<T> {}

impl<T> MainThreadCell<T> {
    /// Wrap `value`; any thread may build the cell.
    pub const fn new(value: T) -> Self {
        Self {
            value: RefCell::new(value),
        }
    }

    /// Shared access on the main thread.
    ///
    /// # Panics
    /// If the value is mutably borrowed (a re-entrant host call).
    pub fn borrow(&self, _: &MainThread) -> Ref<'_, T> {
        self.value.borrow()
    }

    /// Exclusive access on the main thread.
    ///
    /// # Panics
    /// If the value is already borrowed (a re-entrant host call).
    pub fn borrow_mut(&self, _: &MainThread) -> RefMut<'_, T> {
        self.value.borrow_mut()
    }

    /// Exclusive access when re-entry is possible: `None` while borrowed.
    pub fn try_borrow_mut(&self, _: &MainThread) -> Option<RefMut<'_, T>> {
        self.value.try_borrow_mut().ok()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for MainThreadCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MainThreadCell").finish_non_exhaustive()
    }
}

const _: () = {
    const fn assert_sync<T: Sync>() {}
    assert_sync::<MainThreadCell<Box<dyn crate::main_thread::MainThreadHooks>>>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_needs_a_token_and_catches_reentry() {
        let cell = MainThreadCell::new(1);
        // SAFETY: the test thread stands in for the host main thread.
        let main = unsafe { MainThread::assume() };
        *cell.borrow_mut(&main) += 1;
        let held = cell.borrow(&main);
        assert!(cell.try_borrow_mut(&main).is_none(), "re-entry is refused");
        assert_eq!(*held, 2);
    }
}
