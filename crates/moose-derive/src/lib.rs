//! Proc macros for moose plugins.
//!
//! Two distinct macro families live here:
//!
//! - `plugin_info!()` reads `moose.toml` at compile time and expands
//!   to a `PluginInfo` struct literal. Uses toml + serde at
//!   proc-macro compile time.
//! - `#[derive(Params)]`, `#[derive(ParamEnum)]`, `#[derive(State)]`
//!   generate the parameter-discovery / state-roundtrip glue every
//!   plugin needs. Pure syn + quote.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;

mod fields;
mod info;
mod param_enum;
mod param_index;
mod params;
mod state;
mod tokens;

#[proc_macro]
pub fn plugin_info(_input: TokenStream) -> TokenStream {
    info::plugin_info()
}

/// Generate the optional exact VST3 class ID for the current plugin.
#[doc(hidden)]
#[proc_macro]
pub fn plugin_vst3_class_id(_input: TokenStream) -> TokenStream {
    info::plugin_vst3_class_id()
}

/// Write the flattened `param_index.toml` for the plugin whose root
/// params type is `<input>` (the `.preset` field-name table
/// `cargo moose` reads). Invoked by `moose::plugin!`'s expansion; see
/// [`param_index::emit_root_impl`].
#[doc(hidden)]
#[proc_macro]
pub fn __moose_param_index_root(input: TokenStream) -> TokenStream {
    param_index::emit_root_impl(input)
}

// ============================================================================
// Main derive macro
// ============================================================================

/// # Panics
///
/// Panics if `syn` fails to parse the input token stream. That only
/// happens on syntactically broken input (rustc would already be
/// rejecting the same file), so the panic surfaces a derive-internal
/// regression rather than user error.
#[proc_macro_derive(
    Params,
    attributes(param, nested, meter, skip, persist, persist_missing, params)
)]
pub fn derive_params(input: TokenStream) -> TokenStream {
    params::derive_params(input)
}

/// Derive `ParamEnum` for a C-like enum.
///
/// Generates `Clone`, `Copy`, `PartialEq`, `Eq`, and all 5 `ParamEnum`
/// methods: `from_index`, `to_index`, `name`, `variant_count`, and
/// `variant_names`.
///
/// Display names default to the variant identifier. Use `#[name = "..."]`
/// on a variant to override:
///
/// ```ignore
/// #[derive(ParamEnum)]
/// pub enum ArpPattern {
///     Up,
///     Down,
///     #[name = "Up/Down"]
///     UpDown,
///     Random,
/// }
/// ```
/// # Panics
///
/// Panics if `syn` fails to parse the input token stream - same
/// "rustc-already-rejected" condition as [`derive_params`].
#[proc_macro_derive(ParamEnum, attributes(name))]
pub fn derive_param_enum(input: TokenStream) -> TokenStream {
    param_enum::derive_param_enum(input)
}

/// Derive binary serialization for a custom state struct.
///
/// The struct must also implement `Default`. The codec is **keyed by
/// field name**, so schema evolution is safe in every direction: adding,
/// removing, and reordering fields all preserve each surviving field's
/// value. A field absent from a blob loads as its `Default`; an unknown
/// stored field is ignored. Two field names that collide under the key
/// hash are a compile error. Pre-keyed blobs still load via a legacy
/// positional path (existing sessions are unaffected).
///
/// Supported field types: `u8`, `u16`, `u32`, `u64`, `i8`, `i16`, `i32`, `i64`,
/// `f32`, `f64`, `bool`, `String`, `Vec<T>`, `Option<T>`, and nested `State` types.
///
/// ```ignore
/// #[derive(State, Default)]
/// pub struct MyState {
///     pub instance_name: String,
///     pub view_mode: u8,
///     pub selected_ids: Vec<u32>,
/// }
/// ```
/// # Panics
///
/// Panics if `syn` fails to parse the input token stream - same
/// "rustc-already-rejected" condition as [`derive_params`].
#[proc_macro_derive(State)]
pub fn derive_state(input: TokenStream) -> TokenStream {
    state::derive_state(input)
}
