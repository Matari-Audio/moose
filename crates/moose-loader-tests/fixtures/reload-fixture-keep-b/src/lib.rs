//! Reload-transition fixture: `CounterLogic` over `CounterState`.
//!
//! Identical logic to `reload-fixture-keep-a`, exported from a separate
//! crate so the two produce distinct dylib files sharing one
//! `save_state` / `load_state` format - the reload whose live state the
//! shell carries over.

use moose::prelude::*;
use reload_fixture_common::{CounterLogic, FxParams};

moose_loader::export_plugin!(CounterLogic, FxParams);
