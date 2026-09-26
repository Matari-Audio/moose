//! Safe types that cross the dylib boundary.
//!
//! Re-exports types from `moose-core` and `moose-gui-types` so there's
//! ONE definition of each type. No duplication. Sourced from
//! `moose-gui-types` (the lightweight types crate) rather than the
//! heavier `moose-gui` renderer so the canary stays buildable when
//! `builtin-gui` is off.

pub use moose_core::buffer::AudioBuffer;
pub use moose_core::events::{Event, EventBody, EventList, TransportInfo};
pub use moose_core::process::{ProcessContext, ProcessStatus};
pub use moose_gui_types::interaction::WidgetRegion;
pub use moose_gui_types::theme::{Color, Theme};
