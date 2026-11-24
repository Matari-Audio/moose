//! Safe types that cross the dylib boundary.
//!
//! Re-exports types from `moose-core` so there's ONE definition of each type.

pub use moose_core::buffer::AudioBuffer;
pub use moose_core::events::{Event, EventBody, EventList, TransportInfo};
pub use moose_core::process::{ProcessContext, ProcessStatus};
