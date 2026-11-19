//! `#[derive(State)]`: keyed binary codec for custom state.

use crate::fields::state_field_hash;

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields};

// ---------------------------------------------------------------------------
// #[derive(State)] - binary serialization for custom plugin state
// ---------------------------------------------------------------------------

// One linear pass building the keyed serialize / dual-path deserialize
// codegen; splitting it would scatter the token-stream construction.
#[allow(clippy::too_many_lines)]
pub(crate) fn derive_state(input: TokenStream) -> TokenStream {
    let ast: DeriveInput = syn::parse(input).expect("Failed to parse input for State derive");
    let name = &ast.ident;

    let fields = match &ast.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return syn::Error::new_spanned(
                    &ast,
                    "State can only be derived on structs with named fields",
                )
                .to_compile_error()
                .into();
            }
        },
        _ => {
            return syn::Error::new_spanned(&ast, "State can only be derived on structs")
                .to_compile_error()
                .into();
        }
    };

    // `fields` is the syn-parsed field list of a single struct; can't
    // overflow u32. moose-derive is a proc-macro crate so it can't
    // pull in `moose_core::cast::len_u32`.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "struct field count is bounded by syn parser limits"
    )]
    let field_count = fields.len() as u32;
    let field_idents: Vec<_> = fields
        .iter()
        .map(|f| {
            f.ident
                .as_ref()
                .expect("Fields::Named guarantees every field carries an ident")
        })
        .collect();

    // Per-field key for the keyed codec (add/remove/reorder-safe). Reject
    // a same-hash pair at compile time so a collision can never silently
    // mis-assign - the exact footgun the keyed format removes.
    let field_hashes: Vec<u32> = field_idents
        .iter()
        .map(|id| state_field_hash(&id.to_string()))
        .collect();
    {
        let mut seen: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
        for (id, h) in field_idents.iter().zip(&field_hashes) {
            if let Some(prev) = seen.insert(*h, id.to_string()) {
                let msg = format!(
                    "State field name hash collision between `{prev}` and `{id}` \
                     (hash {h:#010x}); rename one field"
                );
                return syn::Error::new_spanned(&ast, msg).to_compile_error().into();
            }
        }
    }

    // Keyed field frame: `[name_hash:4][value_len:4][value bytes]`. The
    // hash lets the reader match by name regardless of declaration order.
    let write_fields: Vec<_> = field_idents
        .iter()
        .zip(&field_hashes)
        .map(|(ident, hash)| {
            quote! {
                {
                    buf.extend_from_slice(&#hash.to_le_bytes());
                    let field_start = buf.len();
                    buf.extend_from_slice(&0u32.to_le_bytes());
                    ::moose::core::custom_state::StateField::write_field(&self.#ident, buf);
                    let field_len = (buf.len() - field_start - 4) as u32;
                    buf[field_start..field_start + 4].copy_from_slice(&field_len.to_le_bytes());
                }
            }
        })
        .collect();

    // Read arm per field: match the stored key hash, read the value into
    // its slot. Unmatched stored fields (a removed field) fall through the
    // `_` arm and are skipped; declared fields with no match keep their
    // `Default`. This is what makes add / remove / reorder safe.
    let keyed_read_arms: Vec<_> = field_idents
        .iter()
        .zip(&field_hashes)
        .map(|(ident, hash)| {
            quote! {
                #hash => {
                    if let Some(val) =
                        ::moose::core::custom_state::StateField::read_field(&mut cursor)
                    {
                        result.#ident = val;
                    }
                }
            }
        })
        .collect::<Vec<_>>();

    // Legacy positional read (pre-keyed blobs): assign the i-th stored
    // field to the i-th declared field. Kept verbatim so old sessions /
    // presets still load; new saves always use the keyed format above.
    let legacy_read_fields: Vec<_> = field_idents.iter().map(|ident| {
        quote! {
            if field_idx < stored_count {
                // `cursor.read_bytes(4)` returns a 4-byte slice when
                // `Some` (or `None` if the cursor is short). The
                // `try_into` to `[u8; 4]` therefore can't fail when
                // we're inside the `if let Some(_)` arm - but
                // routing through `.ok()` instead of `.unwrap()`
                // keeps the panic path closed for the case where a
                // future change to `read_bytes` relaxes its
                // length-guarantee. Matches the `stored_count` site
                // a few lines down.
                if let Some(len_bytes) = cursor.read_bytes(4)
                    && let Ok(len_arr) = <[u8; 4]>::try_from(len_bytes)
                {
                    let field_len = u32::from_le_bytes(len_arr) as usize;
                    let pos_before = cursor.remaining();
                    if let Some(val) = ::moose::core::custom_state::StateField::read_field(&mut cursor) {
                        result.#ident = val;
                    }
                    let consumed = pos_before - cursor.remaining();
                    if consumed < field_len {
                        let _ = cursor.read_bytes(field_len - consumed);
                    }
                }
                field_idx += 1;
            }
        }
    }).collect();

    let expanded = quote! {
        impl ::moose::core::custom_state::State for #name {
            fn serialize_into(&self, buf: &mut Vec<u8>) {
                buf.clear();
                // Keyed format: a leading magic (`0xFFFFFF00 | version`)
                // that a legacy positional blob's small field-count first
                // word can never be, so `deserialize` distinguishes them.
                buf.extend_from_slice(&0xFFFF_FF01u32.to_le_bytes());
                buf.extend_from_slice(&#field_count.to_le_bytes());
                #(#write_fields)*
            }

            fn deserialize(data: &[u8]) -> Option<Self> {
                let mut cursor = ::moose::core::custom_state::StateCursor::new(data);
                let tag_bytes = cursor.read_bytes(4)?;
                let tag = u32::from_le_bytes(tag_bytes.try_into().ok()?);
                let mut result = Self::default();

                if (tag & 0xFFFF_FF00) == 0xFFFF_FF00 {
                    // Keyed format. The low byte is the frame-layout version.
                    // Field evolution (add / remove / reorder / shrink) stays
                    // on v1 - the per-field hash + length absorbs it - so the
                    // version only bumps for an incompatible change to the
                    // frame layout itself. An unknown version is therefore
                    // something this build can't parse: fail cleanly to
                    // defaults rather than misread a v2 frame as v1.
                    if (tag & 0xFF) != 1 {
                        return None;
                    }
                    // Each field is `[name_hash:4][len:4][value]`; match by
                    // hash, so add / remove / reorder are all safe.
                    let count_bytes = cursor.read_bytes(4)?;
                    // Each keyed field is >= 8 bytes (hash + len), so bound
                    // the count by remaining/8 to reject a hostile count.
                    let stored_count = (u32::from_le_bytes(count_bytes.try_into().ok()?) as usize)
                        .min(cursor.remaining() / 8 + #field_count as usize);
                    let mut kf: usize = 0;
                    while kf < stored_count {
                        let Some(hb) = cursor.read_bytes(4) else { break };
                        let Ok(harr) = <[u8; 4]>::try_from(hb) else { break };
                        let hash = u32::from_le_bytes(harr);
                        let Some(lb) = cursor.read_bytes(4) else { break };
                        let Ok(larr) = <[u8; 4]>::try_from(lb) else { break };
                        let field_len = u32::from_le_bytes(larr) as usize;
                        // Consume exactly this field's bytes from the main
                        // cursor up front, then parse them in a sub-cursor
                        // bounded to `field_len`. A field whose *type* changed
                        // (say u32 -> String) reads a garbage length; bounding
                        // it here makes that fail to Default for this field
                        // alone instead of running into - and corrupting -
                        // every field after it. A short/malformed frame stops
                        // the loop.
                        let Some(field_bytes) = cursor.read_bytes(field_len) else { break };
                        // The read arms below read from `cursor`; shadow it
                        // with the bounded sub-cursor for the match. The allow
                        // covers a zero-field struct, where no arm uses it.
                        #[allow(unused_mut, unused_variables)]
                        let mut cursor =
                            ::moose::core::custom_state::StateCursor::new(field_bytes);
                        match hash {
                            #(#keyed_read_arms)*
                            // Unknown key (a since-removed field): the frame is
                            // already consumed from the main cursor.
                            _ => {}
                        }
                        kf += 1;
                    }
                } else {
                    // Legacy positional format: `tag` is the field count.
                    // Assign the i-th stored field to the i-th declared one
                    // (append-only-safe; the keyed path above supersedes it
                    // for anything saved by this build).
                    let stored_count = (tag as usize)
                        .min(cursor.remaining() / 4 + #field_count as usize);
                    let mut field_idx: usize = 0;
                    #(#legacy_read_fields)*
                    while field_idx < stored_count {
                        if !cursor.skip_field() { break; }
                        field_idx += 1;
                    }
                }
                Some(result)
            }
        }

        // Also usable as a single length-prefixed `StateField`, so a
        // `#[derive(State)]` struct can be a `#[persist]` field's inner
        // type (`RwLock<MyState>`) or nest inside another state struct.
        impl ::moose::core::custom_state::StateField for #name {
            fn write_field(&self, buf: &mut Vec<u8>) {
                let start = buf.len();
                buf.extend_from_slice(&0u32.to_le_bytes());
                let mut inner = Vec::new();
                <Self as ::moose::core::custom_state::State>::serialize_into(self, &mut inner);
                buf.extend_from_slice(&inner);
                let len = (buf.len() - start - 4) as u32;
                buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
            }
            fn read_field(
                cursor: &mut ::moose::core::custom_state::StateCursor,
            ) -> Option<Self> {
                let len_bytes = cursor.read_bytes(4)?;
                let len = u32::from_le_bytes(<[u8; 4]>::try_from(len_bytes).ok()?) as usize;
                let bytes = cursor.read_bytes(len)?;
                <Self as ::moose::core::custom_state::State>::deserialize(bytes)
            }
        }
    };

    expanded.into()
}
