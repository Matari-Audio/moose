//! The atomics behind the crate's lock-free handoffs ([`crate::transport`]'s
//! seqlock and the wrapper's plugin cell). A `loom` test build swaps in
//! loom's model-checked atomics, so the loom tests exercise the real code
//! rather than a copy of it. Run them with
//! `cargo test -p moose-core --features loom --lib loom`; under that build
//! the other tests that touch these types can't run outside `loom::model`.

#[cfg(all(test, feature = "loom"))]
pub(crate) use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
#[cfg(not(all(test, feature = "loom")))]
pub(crate) use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
