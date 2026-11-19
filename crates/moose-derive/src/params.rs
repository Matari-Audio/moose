//! `#[derive(Params)]`.

use crate::fields::IdScheme;
use crate::fields::MidiBindKind;
use crate::fields::ParamField;
use crate::fields::ParamKind;
use crate::fields::collect_fields;
use crate::fields::has_persist_attr;
use crate::fields::has_persist_missing_attr;
use crate::fields::name_hash_id;
use crate::fields::parse_params_struct_attrs;
use crate::fields::persist_key;
use crate::param_index;
use crate::tokens::gen_field_constructor;
use crate::tokens::gen_param_info_literal;

use moose_params::METER_ID_BASE;
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use std::collections::HashSet;
use syn::ext::IdentExt;
use syn::{Data, DeriveInput, Fields};

#[allow(clippy::too_many_lines)]
pub(crate) fn derive_params(input: TokenStream) -> TokenStream {
    let ast: DeriveInput = syn::parse(input).expect("Failed to parse input for Params derive");
    let struct_name = &ast.ident;

    let fields = match &ast.data {
        Data::Struct(data) => &data.fields,
        _ => {
            return syn::Error::new_spanned(&ast, "Params can only be derived on structs")
                .to_compile_error()
                .into();
        }
    };

    let (mut param_fields, nested_fields, mut meter_fields, skip_fields) =
        match collect_fields(fields) {
            Ok(fields) => fields,
            Err(e) => return e.to_compile_error().into(),
        };

    // `#[persist]` fields are non-params the host saves alongside param
    // values (editor-editable config). Also present in `skip_fields` so
    // `new()` `Default`-constructs them; collected here (ident + key) for
    // the serialize/load-persist codegen.
    let persist_fields: Vec<(syn::Ident, String, bool)> = match fields {
        Fields::Named(named) => named
            .named
            .iter()
            .filter(|f| has_persist_attr(f))
            .filter_map(|f| {
                f.ident
                    .clone()
                    .map(|id| (id, persist_key(f), has_persist_missing_attr(f)))
            })
            .collect(),
        _ => Vec::new(),
    };

    // A persistence-only subtree still needs constructor/offset and traversal
    // code. Explicit skipped fields (and an empty named store) are useful for
    // lifecycle hooks without exposing a fake host parameter.
    let empty_named = matches!(fields, Fields::Named(named) if named.named.is_empty());
    if param_fields.is_empty()
        && nested_fields.is_empty()
        && meter_fields.is_empty()
        && skip_fields.is_empty()
        && !empty_named
    {
        return syn::Error::new_spanned(
            &ast,
            "Params derive: no recognized fields (FloatParam, BoolParam, IntParam, EnumParam, MeterSlot, #[nested], #[persist], or #[skip])",
        )
        .to_compile_error()
        .into();
    }

    // --- Auto-assign parameter IDs ---
    // Explicit `#[param(id = N)]` always wins. Un-pinned params get an
    // ID per the struct's `id_scheme`: a stable hash of the field name
    // (default), or a sequential counter in field order (legacy
    // `#[params(id_scheme = "ordinal")]`). See `IdScheme`.
    let struct_attrs = match parse_params_struct_attrs(&ast.attrs) {
        Ok(attrs) => attrs,
        Err(e) => return e.to_compile_error().into(),
    };
    let scheme = struct_attrs.id_scheme;
    match scheme {
        IdScheme::Ordinal => {
            let explicit_ids: HashSet<u32> =
                param_fields.iter().filter_map(|f| f.attrs.id).collect();
            let mut next_auto = 0u32;
            for f in &mut param_fields {
                if f.attrs.id.is_none() {
                    while explicit_ids.contains(&next_auto) {
                        next_auto += 1;
                    }
                    f.attrs.id = Some(next_auto);
                    next_auto += 1;
                }
            }
        }
        IdScheme::Hash => {
            // Hash the field ident (not the `name =` display string,
            // which changes for UX reasons). Collisions - hash-vs-hash
            // or hash-vs-explicit - are caught by the duplicate-ID check
            // below; the author resolves by pinning one with `id = N`.
            for f in &mut param_fields {
                if f.attrs.id.is_none() {
                    f.attrs.id = Some(name_hash_id(&f.path()));
                }
            }
        }
    }

    // --- Auto-assign meter IDs ---
    // Meters live outside the signed-positive 31-bit host parameter
    // domain. Storage indexes as `meter_array[id - METER_ID_BASE]`.
    // `METER_ID_BASE` is imported from `moose_params` at proc-macro
    // build time so the value can't drift between crates.
    for (next_meter, m) in (METER_ID_BASE..).zip(meter_fields.iter_mut()) {
        m.id = Some(next_meter);
    }

    // --- Compile-time validation: duplicate IDs + host-safe range ---
    //
    // Checks:
    //  1. No two params share an ID.
    //  2. Every explicit param ID is in the host-safe 31-bit domain.
    //  3. No param ID collides with any meter ID (follows from #2 when
    //     both checks pass, but surfaced separately for a clearer error).
    {
        let mut seen_ids = HashSet::new();
        for f in &param_fields {
            if let Some(id) = f.attrs.id {
                if id >= METER_ID_BASE {
                    let msg = format!(
                        "Parameter ID {id} is outside the host-safe 31-bit range. \
                         Param IDs must be <= {}.",
                        ::moose_params::PARAM_ID_MAX
                    );
                    return syn::Error::new_spanned(&ast, msg).to_compile_error().into();
                }
                if !seen_ids.insert(id) {
                    let msg = format!("Duplicate parameter ID: {id}");
                    return syn::Error::new_spanned(&ast, msg).to_compile_error().into();
                }
            }
        }
        for m in &meter_fields {
            if let Some(id) = m.id
                && !seen_ids.insert(id)
            {
                let msg = format!("Meter ID {id} collides with a parameter ID.");
                return syn::Error::new_spanned(&ast, msg).to_compile_error().into();
            }
        }
    }

    // Keep existing wire IDs: sparse ordinal parents must pin nested bases
    // rather than implicitly packing children into potentially occupied IDs.
    if matches!(scheme, IdScheme::Ordinal)
        && nested_fields.iter().any(|n| n.base.is_none())
        && param_fields
            .iter()
            .any(|f| f.id() as usize >= param_fields.len())
    {
        return syn::Error::new_spanned(
            &ast,
            "ordinal Params with sparse parent IDs require explicit #[nested(base = N)] bases",
        )
        .to_compile_error()
        .into();
    }

    // Variant conversion can fold distinct field paths to the same spelling,
    // including array elements and meters. Diagnose both original paths.
    let mut variants = std::collections::HashMap::new();
    for (variant, path, ident) in param_fields
        .iter()
        .map(|f| (f.variant(), f.path(), &f.ident))
        .chain(
            meter_fields
                .iter()
                .map(|m| (snake_to_pascal(&m.ident), m.ident.to_string(), &m.ident)),
        )
    {
        if let Some(previous) = variants.insert(variant.to_string(), path.clone()) {
            return syn::Error::new_spanned(
                ident,
                format!(
                    "fields `{previous}` and `{path}` generate the same ParamId variant `{variant}`"
                ),
            )
            .to_compile_error()
            .into();
        }
    }

    // MIDI bindings must be unambiguous: two params can't claim the same
    // source on overlapping channels (an any-channel binding overlaps
    // every channel), or the host's mapping query would be
    // order-dependent.
    {
        let bound: Vec<(&str, MidiBindKind, Option<u8>)> = param_fields
            .iter()
            .filter_map(|f| {
                f.attrs.midi_map.map(|k| {
                    (
                        f.attrs.name.as_deref().unwrap_or("<unnamed>"),
                        k,
                        f.attrs.midi_channel,
                    )
                })
            })
            .collect();
        for (i, (na, ka, cha)) in bound.iter().enumerate() {
            for (nb, kb, chb) in bound.iter().skip(i + 1) {
                let overlap = cha.is_none() || chb.is_none() || cha == chb;
                if ka == kb && overlap {
                    let msg = format!(
                        "conflicting MIDI binding: parameters `{na}` and `{nb}` map the \
                         same source on overlapping channels"
                    );
                    return syn::Error::new_spanned(&ast, msg).to_compile_error().into();
                }
            }
        }
    }

    // `#[persist]` keys and `#[nested]` field names share one keyed
    // list in the saved state (`serialize_persist` / `load_persist`),
    // so a duplicate key would generate two match arms for the same
    // string and the later entry would silently never load. Bare
    // `#[persist]` uses the unique field name, so a collision needs an
    // explicit `#[persist = "key"]` - reject it here with the field
    // that introduced the duplicate.
    {
        let mut seen_keys = HashSet::new();
        let all_keys = persist_fields
            .iter()
            .map(|(ident, key, _)| (ident, key.clone()))
            .chain(
                nested_fields
                    .iter()
                    .map(|n| (&n.ident, n.ident.to_string())),
            );
        for (ident, key) in all_keys {
            if !seen_keys.insert(key.clone()) {
                let msg = format!(
                    "duplicate persist key `{key}`: `#[persist]` keys and `#[nested]` \
                     field names share one saved-state list and must be unique"
                );
                return syn::Error::new(ident.span(), msg).to_compile_error().into();
            }
        }
    }

    // Stable string IDs (D9) key OAST v2 state and `ParamCatalog`'s side
    // map. A duplicate would make the runtime catalog drop one of them, so
    // reject it here; keys from nested structs are checked by their own
    // derive and namespaced by the field name.
    {
        let keys: Vec<String> = param_fields.iter().map(ParamField::string_key).collect();
        let nested: Vec<String> = nested_fields.iter().map(|n| n.ident.to_string()).collect();
        if let Some(msg) = string_key_conflict(&keys, &nested) {
            return syn::Error::new_spanned(&ast, msg).to_compile_error().into();
        }
    }

    // --- Compile-time param-index sidecar ---
    //
    // Each Params struct writes `<target>/param-index/<crate>/<struct>
    // .params.toml`; `__moose_param_index_root!` (from `moose::plugin!`)
    // flattens the tree for the preset tooling. Failures here are
    // silent - they surface when the root aggregates.
    let nested_for_sidecar: Vec<(syn::Ident, syn::Type, Option<u32>)> = nested_fields
        .iter()
        .map(|n| (n.ident.clone(), n.ty.clone(), n.base))
        .collect();
    param_index::write_struct_sidecar(
        struct_name,
        scheme == IdScheme::Hash,
        struct_attrs.projection.is_some(),
        &param_fields,
        &nested_for_sidecar,
    );

    // --- Always generate new() ---
    // Nested structs need `new()` too: it constructs each child and
    // rebases its ids by the group's base, so reusing one Params type in
    // two `#[nested]` slots yields disjoint id ranges instead of a clash.
    let generate_new = !param_fields.is_empty()
        || !meter_fields.is_empty()
        || !nested_fields.is_empty()
        || !skip_fields.is_empty()
        || empty_named;

    // --- Count ---
    let own_count = param_fields.len();
    let nested_idents: Vec<_> = nested_fields.iter().map(|n| &n.ident).collect();
    let count_expr = if nested_fields.is_empty() {
        quote! { #own_count }
    } else {
        quote! { #own_count #(+ self.#nested_idents.count())* }
    };

    // --- param_infos ---
    let own_infos: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = f.access();
            if f.kind == ParamKind::ValueList {
                quote! { self.#ident.current_info() }
            } else {
                quote! { self.#ident.info() }
            }
        })
        .collect();

    let infos_expr = if nested_fields.is_empty() {
        quote! { vec![#(#own_infos),*] }
    } else {
        // Recurse via `append_param_infos` so each nested struct
        // pushes directly into the shared buffer instead of building
        // its own `Vec` that the outer call then extends. Saves
        // O(depth) intermediate allocations per `param_infos()` call.
        quote! {
            let mut infos = vec![#(#own_infos),*];
            #(self.#nested_idents.append_param_infos(&mut infos);)*
            infos
        }
    };

    // Override `append_param_infos` so the buffer-based form
    // recurses without the extra `Vec` round-trip in nested cases.
    // Plain (non-nested) structs accept the default impl.
    let append_infos_impl = if nested_fields.is_empty() {
        quote! {}
    } else {
        quote! {
            fn append_param_infos(&self, into: &mut Vec<::moose::params::ParamInfo>) {
                #(into.push(#own_infos);)*
                #(self.#nested_idents.append_param_infos(into);)*
            }
        }
    };

    // --- param_infos_static ---
    // Same shape as `param_infos`, but each entry is the raw
    // `ParamInfo { ... }` literal (built by
    // `gen_param_info_literal`) rather than a runtime `self.<f>.info()`
    // read, so format wrappers' `register_*` paths can read parameter
    // metadata without constructing a plugin instance.
    let own_info_literals: Vec<proc_macro2::TokenStream> = param_fields
        .iter()
        .filter_map(gen_param_info_literal)
        .collect();
    let nested_static_calls: Vec<proc_macro2::TokenStream> = nested_fields
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let ty = &n.ty;
            // Base/salt for the group, by scheme (an explicit
            // `#[nested(base=N)]` overrides either):
            //  - ordinal: auto-packed right after the own params and
            //    every earlier nested group's span (contiguous ranges).
            //  - hash: a stable hash of the slot field name, so the
            //    group's ids don't shift when nested groups reorder.
            // The child's ids are then folded in additively and masked
            // back into the param range; the constructor's `offset_ids`
            // does the same fold, so both paths agree.
            let base_expr = if let Some(b) = n.base {
                quote! { #b }
            } else {
                match scheme {
                    IdScheme::Ordinal => {
                        let prev = nested_fields[..i].iter().map(|p| &p.ty);
                        quote! {
                            (#own_count as u32)
                            #(+ <#prev as ::moose::params::Params>::param_infos_static().len() as u32)*
                        }
                    }
                    IdScheme::Hash => {
                        let salt = name_hash_id(&n.ident.to_string());
                        quote! { #salt }
                    }
                }
            };
            quote! {
                {
                    let __base: u32 = #base_expr;
                    infos.extend(
                        <#ty as ::moose::params::Params>::param_infos_static()
                            .into_iter()
                            .map(|mut __info| {
                                __info.id = ::moose::params::rebase_nested_param_id(
                                    __info.id,
                                    __base,
                                );
                                __info
                            }),
                    );
                }
            }
        })
        .collect();
    let static_infos_body = if nested_fields.is_empty() {
        quote! { vec![#(#own_info_literals),*] }
    } else {
        quote! {
            {
                let mut infos: Vec<::moose::params::ParamInfo> = vec![#(#own_info_literals),*];
                #(#nested_static_calls)*
                infos
            }
        }
    };
    let static_infos_body = if let Some(provider) = &struct_attrs.projection {
        quote! {
            {
                let mut infos = { #static_infos_body };
                ::moose::params::append_projected_infos::<#struct_name, #provider>(&mut infos);
                infos
            }
        }
    } else {
        static_infos_body
    };
    let param_infos_static_impl = quote! {
        fn param_infos_static() -> Vec<::moose::params::ParamInfo>
        where
            Self: ::std::marker::Sized,
        {
            // `LazyLock` so the first call computes the metadata and
            // every later registration reads the cache. `clone()` is
            // a single Vec allocation - cheap relative to the avoided
            // plugin construction. (`ParamInfo` is `Clone`.)
            static INFOS: ::std::sync::LazyLock<Vec<::moose::params::ParamInfo>> =
                ::std::sync::LazyLock::new(|| #static_infos_body);
            INFOS.clone()
        }
    };

    // --- meter_ids ---
    let own_meter_ids: Vec<_> = meter_fields
        .iter()
        .map(|m| {
            let ident = &m.ident;
            quote! { self.#ident.id() }
        })
        .collect();
    let meter_ids_expr = if nested_fields.is_empty() {
        quote! { vec![#(#own_meter_ids),*] }
    } else {
        quote! {
            let mut ids = vec![#(#own_meter_ids),*];
            #(ids.extend(self.#nested_idents.meter_ids());)*
            ids
        }
    };

    // --- get_plain ---
    // One body per field feeds both the per-id arms and the D6 per-index
    // arms, so the two lookups can never disagree.
    let get_plain_bodies: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = f.access();
            let body = match f.kind {
                ParamKind::Float => quote! { Some(self.#ident.raw_target()) },
                // `i64 as f64` is precision-lossy by spec (mantissa 53 < 63);
                // no `From<i64> for f64` exists, so the cast is the idiom.
                ParamKind::Int => quote! {{
                    #[allow(clippy::cast_precision_loss)]
                    let v = self.#ident.value() as f64;
                    Some(v)
                }},
                ParamKind::Bool => quote! { Some(if self.#ident.value() { 1.0 } else { 0.0 }) },
                // `u32 → f64` is lossless (u32::MAX < 2^53).
                ParamKind::Enum => quote! { Some(f64::from(self.#ident.index())) },
                ParamKind::ValueList => quote! { Some(self.#ident.get_plain()) },
            };
            (ident, body)
        })
        .collect();
    let get_plain_arms = id_arms(&get_plain_bodies);
    let get_plain_by_index_arms = index_arms(&get_plain_bodies);

    let get_plain_fallthrough = if nested_fields.is_empty() {
        quote! { _ => None, }
    } else {
        quote! {
            _ => {
                #(if let Some(v) = self.#nested_idents.get_plain(id) { return Some(v); })*
                None
            }
        }
    };

    let get_plain_by_index_fallthrough = if nested_fields.is_empty() {
        quote! { _ => <Self as ::moose::params::Params>::get_plain(self, id), }
    } else {
        quote! {
            _ => {
                if let Some(mut __rel) = index.checked_sub(#own_count) {
                    'nested: {
                        #(
                            let __n = self.#nested_idents.count();
                            if __rel < __n {
                                if let Some(__v) = self.#nested_idents.get_plain_by_index(__rel, id) {
                                    return Some(__v);
                                }
                                break 'nested;
                            }
                            __rel -= __n;
                        )*
                    }
                }
                <Self as ::moose::params::Params>::get_plain(self, id)
            }
        }
    };

    // --- get_normalized ---
    //
    // Per-id match arms reach into the matching param's `info.range`
    // and call `normalize` / `denormalize` directly. Dispatching
    // through `self.param_infos()` would allocate a `Vec<ParamInfo>`
    // on every host-driven `set_normalized` / `get_normalized` round
    // trip and every `EditorBridge` paint frame.
    let get_normalized_arms: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = f.access();
            if f.kind == ParamKind::ValueList {
                return quote! {
                    x if x == self.#ident.id() => Some(self.#ident.get_normalized()),
                };
            }
            let plain_expr = match f.kind {
                ParamKind::Float => quote! { self.#ident.raw_target() },
                // i64 → f64 has no `From`; `as` with an explicit
                // allow is the idiom.
                ParamKind::Int => quote! {{
                    #[allow(clippy::cast_precision_loss)]
                    let v = self.#ident.value() as f64;
                    v
                }},
                ParamKind::Bool => quote! { if self.#ident.value() { 1.0 } else { 0.0 } },
                // u32 → f64 is lossless: use `From`.
                ParamKind::Enum => quote! { f64::from(self.#ident.index()) },
                ParamKind::ValueList => unreachable!(),
            };
            quote! {
                x if x == self.#ident.id() => Some(self.#ident.range().normalize(#plain_expr)),
            }
        })
        .collect();

    let get_normalized_fallthrough = if nested_fields.is_empty() {
        quote! { _ => None, }
    } else {
        quote! {
            _ => {
                #(if let Some(v) = self.#nested_idents.get_normalized(id) { return Some(v); })*
                None
            }
        }
    };

    // --- set_plain ---
    let set_plain_bodies: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = f.access();
            let body = match f.kind {
                ParamKind::Float => quote! { self.#ident.set_value(value) },
                ParamKind::Bool => quote! { self.#ident.set_value(value > 0.5) },
                ParamKind::Int => quote! { self.#ident.set_value(value.round() as i64) },
                ParamKind::Enum => quote! { self.#ident.set_index(value.round() as u32) },
                ParamKind::ValueList => quote! { self.#ident.set_plain(value) },
            };
            (ident, body)
        })
        .collect();
    let set_plain_arms = id_arms(&set_plain_bodies);
    let set_plain_by_index_arms = index_arms(&set_plain_bodies);

    let set_plain_fallthrough = if nested_fields.is_empty() {
        quote! { _ => {} }
    } else {
        quote! {
            _ => {
                #(self.#nested_idents.set_plain(id, value);)*
            }
        }
    };

    let set_plain_by_index_fallthrough = if nested_fields.is_empty() {
        quote! { _ => <Self as ::moose::params::Params>::set_plain(self, id, value), }
    } else {
        quote! {
            _ => {
                if let Some(mut __rel) = index.checked_sub(#own_count) {
                    'nested: {
                        #(
                            let __n = self.#nested_idents.count();
                            if __rel < __n {
                                if self.#nested_idents.get_plain_by_index(__rel, id).is_some() {
                                    self.#nested_idents.set_plain_by_index(__rel, id, value);
                                    return;
                                }
                                break 'nested;
                            }
                            __rel -= __n;
                        )*
                    }
                }
                <Self as ::moose::params::Params>::set_plain(self, id, value);
            }
        }
    };

    // --- set_normalized ---
    //
    // Per-id arms denormalize through the matching param's range, then
    // commit through the kind-specific atomic write. Same allocation
    // motivation as `get_normalized` above.
    let set_normalized_arms: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = f.access();
            if f.kind == ParamKind::ValueList {
                return quote! {
                    x if x == self.#ident.id() => self.#ident.set_normalized(value),
                };
            }
            let commit = match f.kind {
                ParamKind::Float => quote! { self.#ident.set_value(plain) },
                ParamKind::Bool => quote! { self.#ident.set_value(plain > 0.5) },
                ParamKind::Int => quote! { self.#ident.set_value(plain.round() as i64) },
                ParamKind::Enum => quote! { self.#ident.set_index(plain.round() as u32) },
                ParamKind::ValueList => unreachable!(),
            };
            quote! {
                x if x == self.#ident.id() => {
                    let plain = self.#ident.range().denormalize(value);
                    #commit;
                }
            }
        })
        .collect();

    let set_normalized_fallthrough = if nested_fields.is_empty() {
        quote! { _ => {} }
    } else {
        quote! {
            _ => {
                #(self.#nested_idents.set_normalized(id, value);)*
            }
        }
    };

    // --- format_value ---
    let format_value_bodies: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = f.access();
            let body = if let Some(ref fmt_fn) = f.attrs.format_fn {
                let fmt_ident = syn::Ident::new(fmt_fn, f.ident.span());
                quote! { Some(self.#fmt_ident(value)) }
            } else {
                match f.kind {
                    ParamKind::Bool => quote! {
                        Some(if value > 0.5 { "On".to_string() } else { "Off".to_string() })
                    },
                    ParamKind::Enum => {
                        let enum_ty = f
                            .enum_type
                            .as_ref()
                            .expect("ParamKind::Enum field must have enum_type populated");
                        quote! { Some(::moose::params::EnumParam::<#enum_ty>::format_by_index(value)) }
                    }
                    ParamKind::ValueList => quote! { Some(self.#ident.format_value(value)) },
                    _ => quote! {
                        Some(::moose::params::format_param_value(self.#ident.static_info(), value))
                    },
                }
            };
            (ident, body)
        })
        .collect();
    let format_value_arms = id_arms(&format_value_bodies);
    let format_value_by_index_arms = index_arms(&format_value_bodies);

    let format_fallthrough = if nested_fields.is_empty() {
        quote! { _ => None, }
    } else {
        quote! {
            _ => {
                #(if let Some(v) = self.#nested_idents.format_value(id, value) { return Some(v); })*
                None
            }
        }
    };

    let format_by_index_fallthrough = if nested_fields.is_empty() {
        quote! { _ => <Self as ::moose::params::Params>::format_value(self, id, value), }
    } else {
        quote! {
            _ => {
                if let Some(mut __rel) = index.checked_sub(#own_count) {
                    'nested: {
                        #(
                            let __n = self.#nested_idents.count();
                            if __rel < __n {
                                if let Some(__s) =
                                    self.#nested_idents.format_value_by_index(__rel, id, value)
                                {
                                    return Some(__s);
                                }
                                break 'nested;
                            }
                            __rel -= __n;
                        )*
                    }
                }
                <Self as ::moose::params::Params>::format_value(self, id, value)
            }
        }
    };

    // --- parse_value ---
    // A `parse_fn` wins; every other param round-trips its display text
    // through `parse_formatted_value` with the same formatter
    // `format_value` uses, so host text entry works for the whole set.
    let parse_value_bodies: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = f.access();
            if let Some(parse_fn) = f.attrs.parse_fn.as_ref() {
                let parse_ident = syn::Ident::new(parse_fn, f.ident.span());
                return (ident, quote! { self.#parse_ident(text) });
            }
            if f.kind == ParamKind::ValueList && f.attrs.format_fn.is_none() {
                let body = quote! { self.#ident.parse_value(text) };
                return (ident, body);
            }
            let (format, lenient) = if let Some(format_fn) = f.attrs.format_fn.as_ref() {
                let format_ident = syn::Ident::new(format_fn, f.ident.span());
                (quote! { |value| self.#format_ident(value) }, false)
            } else {
                let format = match f.kind {
                    ParamKind::Bool => quote! { |value: f64| {
                        if value > 0.5 { "On".to_string() } else { "Off".to_string() }
                    } },
                    ParamKind::Enum => {
                        let enum_ty = f
                            .enum_type
                            .as_ref()
                            .expect("ParamKind::Enum field must have enum_type populated");
                        quote! { |value| ::moose::params::EnumParam::<#enum_ty>::format_by_index(value) }
                    }
                    _ => quote! { |value| ::moose::params::format_param_value(self.#ident.static_info(), value) },
                };
                (format, true)
            };
            let info = if f.kind == ParamKind::ValueList {
                quote! { &self.#ident.current_info() }
            } else {
                quote! { self.#ident.static_info() }
            };
            let body = quote! {
                ::moose::params::parse_formatted_value(
                    #info,
                    text,
                    #format,
                    #lenient,
                )
            };
            (ident, body)
        })
        .collect();
    let parse_value_arms = id_arms(&parse_value_bodies);
    let parse_value_by_index_arms = index_arms(&parse_value_bodies);

    let parse_by_index_fallthrough = if nested_fields.is_empty() {
        quote! { _ => <Self as ::moose::params::Params>::parse_value(self, id, text), }
    } else {
        quote! {
            _ => {
                if let Some(mut __rel) = index.checked_sub(#own_count) {
                    'nested: {
                        #(
                            let __n = self.#nested_idents.count();
                            if __rel < __n {
                                if let Some(__v) =
                                    self.#nested_idents.parse_value_by_index(__rel, id, text)
                                {
                                    return Some(__v);
                                }
                                break 'nested;
                            }
                            __rel -= __n;
                        )*
                    }
                }
                <Self as ::moose::params::Params>::parse_value(self, id, text)
            }
        }
    };

    let nested_parse = if nested_fields.is_empty() {
        quote! { _ => None, }
    } else {
        quote! {
            _ => {
                #(if let Some(v) = self.#nested_idents.parse_value(id, text) { return Some(v); })*
                None
            }
        }
    };
    let parse_value_impl = quote! {
        match id {
            #(#parse_value_arms)*
            #nested_parse
        }
    };

    // --- snap_smoothers ---
    let snap_stmts: Vec<_> = param_fields
        .iter()
        .filter(|f| f.kind == ParamKind::Float)
        .map(|f| {
            let ident = f.access();
            quote! { self.#ident.snap_smoother(); }
        })
        .collect();

    // --- set_modulation / clear_modulations (CLAP mono mod, D3) ---
    // Only `FloatParam` carries an offset; other kinds and unknown ids fall
    // through to the nested groups, which ignore ids they don't own.
    let modulation_bodies: Vec<_> = param_fields
        .iter()
        .filter(|f| f.kind == ParamKind::Float)
        .map(|f| {
            let ident = f.access();
            (ident.clone(), quote! { self.#ident.set_modulation(amount) })
        })
        .collect();
    let modulation_arms = id_arms(&modulation_bodies);
    let clear_modulation_stmts: Vec<_> = modulation_bodies
        .iter()
        .map(|(ident, _)| quote! { self.#ident.clear_modulation(); })
        .collect();

    // --- set_sample_rate ---
    let sr_stmts: Vec<_> = param_fields
        .iter()
        .filter(|f| f.kind == ParamKind::Float)
        .map(|f| {
            let ident = f.access();
            quote! { self.#ident.set_sample_rate(sample_rate); }
        })
        .collect();

    // --- collect_values ---
    let collect_ids: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = f.access();
            quote! { self.#ident.id() }
        })
        .collect();

    // --- Generate new() ---
    let new_impl = if generate_new {
        // An array's elements are consecutive: one `field: [..]` init.
        let mut param_inits: Vec<TokenStream2> = Vec::new();
        let mut slot = 0;
        while let Some(f) = param_fields.get(slot) {
            let ident = &f.ident;
            if f.index.is_some() {
                let elements: Vec<_> = (slot..)
                    .zip(&param_fields[slot..])
                    .take_while(|(_, e)| e.ident == *ident && e.index.is_some())
                    .map(|(s, e)| gen_field_constructor(e, s))
                    .collect();
                slot += elements.len();
                param_inits.push(quote! { #ident: [#(#elements),*] });
            } else {
                let constructor = gen_field_constructor(f, slot);
                param_inits.push(quote! { #ident: #constructor });
                slot += 1;
            }
        }

        // One shared metadata table per struct type: every instance
        // (including each `#[nested]` slot reusing the type) borrows its
        // `ParamInfo` from here and carries only its own rebased id.
        // `LazyLock` because `default` / `range` may be non-const
        // expressions (`variant_count()`, a const path).
        let info_table = if param_fields.is_empty() {
            quote! {}
        } else {
            let table_len = param_fields.len();
            let literals = param_fields.iter().map(|f| {
                gen_param_info_literal(f).unwrap_or_else(|| {
                    let msg = format!("invalid `#[param]` attributes on field `{}`", f.ident);
                    quote! { compile_error!(#msg) }
                })
            });
            quote! {
                static __MOOSE_PARAM_INFOS: ::std::sync::LazyLock<
                    [::moose::params::ParamInfo; #table_len],
                > = ::std::sync::LazyLock::new(|| [#(#literals),*]);
            }
        };

        let nested_inits: Vec<_> = nested_fields
            .iter()
            .map(|f| {
                let ident = &f.ident;
                quote! { #ident: ::core::default::Default::default() }
            })
            .collect();

        let meter_inits: Vec<_> = meter_fields
            .iter()
            .map(|m| {
                let ident = &m.ident;
                let id = m.id();
                quote! { #ident: ::moose::params::MeterSlot { id: #id } }
            })
            .collect();

        // `#[skip]` fields aren't parameters - default-construct them
        // so the struct literal is complete. Everything else (ids,
        // infos, state, count) ignores them.
        let skip_inits: Vec<_> = skip_fields
            .iter()
            .map(|ident| quote! { #ident: ::core::default::Default::default() })
            .collect();

        // Rebase each nested group by its base, after construction (the
        // child built itself at its own local ids). The base is explicit
        // or auto-packed after the own params and each earlier group.
        let offset_stmts: Vec<_> = nested_fields
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let ident = &n.ident;
                let base_expr = if let Some(b) = n.base {
                    quote! { #b }
                } else {
                    match scheme {
                        IdScheme::Ordinal => {
                            let prev = nested_fields[..i].iter().map(|p| &p.ident);
                            quote! {
                                (#own_count as u32)
                                #(+ ::moose::params::Params::count(&me.#prev) as u32)*
                            }
                        }
                        IdScheme::Hash => {
                            let salt = name_hash_id(&n.ident.to_string());
                            quote! { #salt }
                        }
                    }
                };
                quote! { me.#ident.offset_ids(#base_expr); }
            })
            .collect();
        let me_binding = quote! { let mut me };

        quote! {
            impl #struct_name {
                pub fn new() -> Self {
                    #info_table
                    #me_binding = Self {
                        #(#param_inits,)*
                        #(#nested_inits,)*
                        #(#meter_inits,)*
                        #(#skip_inits,)*
                    };
                    #(#offset_stmts)*
                    <Self as ::moose::params::Params>::set_edit_epoch(
                        &mut me,
                        ::std::sync::Arc::new(::std::sync::atomic::AtomicU64::new(0)),
                    );
                    // The per-struct compile-time ID check can't see
                    // across nested types; a parent id matching a
                    // (rebased) nested id would silently corrupt state
                    // round-trip. Surface it as a construction panic.
                    <Self as ::moose::params::Params>::assert_no_id_collisions(&me);
                    me
                }
            }

            impl Default for #struct_name {
                fn default() -> Self {
                    Self::new()
                }
            }
        }
    } else {
        quote! {}
    };

    // Fold `id_base` into every parameter id in this subtree: own
    // params directly, nested groups by recursing with the same base
    // (their own local spans were already applied at construction).
    // Historical-domain IDs fold as `(id + base) & AUTO_PARAM_ID_MASK`,
    // preserving every existing ordinal/hash/nested ID. Wider explicit
    // IDs are absolute and stay unchanged through every parent. Meters
    // keep their dedicated id range and aren't shifted. Always emitted
    // so a parent can rebase any nested child, leaf or not.
    let own_param_idents: Vec<_> = param_fields.iter().map(ParamField::access).collect();
    let parameter_epoch_expr = if let Some(field) = own_param_idents.first() {
        quote! { self.#field.parameter_edit_epoch() }
    } else {
        quote! {
            #(if self.#nested_idents.count() != 0 {
                return self.#nested_idents.parameter_edit_epoch();
            })*
            0
        }
    };
    let parameter_epoch_impl = quote! {
        fn parameter_edit_epoch(&self) -> u64 { #parameter_epoch_expr }
        fn set_edit_epoch(&mut self, epoch: ::std::sync::Arc<::std::sync::atomic::AtomicU64>) {
            #(self.#own_param_idents.set_edit_epoch(::std::sync::Arc::clone(&epoch));)*
            #(self.#nested_idents.set_edit_epoch(::std::sync::Arc::clone(&epoch));)*
        }
    };
    let offset_ids_impl = quote! {
        impl #struct_name {
            #[doc(hidden)]
            pub fn offset_ids(&mut self, id_base: u32) {
                #(self.#own_param_idents.offset_id(id_base);)*
                #(self.#nested_idents.offset_ids(id_base);)*
            }
        }
    };

    // --- Generate ParamId enum (includes both params and meters) ---
    let param_id_enum = if !param_fields.is_empty() || !meter_fields.is_empty() {
        let enum_name = syn::Ident::new(
            &format!("{}ParamId", struct_name.unraw()),
            struct_name.span(),
        );

        let param_variants: Vec<_> = param_fields
            .iter()
            .map(|f| {
                let variant = f.variant();
                let id_lit = proc_macro2::Literal::u32_unsuffixed(f.id());
                quote! { #variant = #id_lit }
            })
            .collect();

        let meter_variants: Vec<_> = meter_fields
            .iter()
            .map(|m| {
                let variant = snake_to_pascal(&m.ident);
                let id_lit = proc_macro2::Literal::u32_unsuffixed(m.id());
                quote! { #variant = #id_lit }
            })
            .collect();

        let variants: Vec<_> = param_variants
            .iter()
            .chain(meter_variants.iter())
            .cloned()
            .collect();

        let from_u32_arms: Vec<_> = param_fields
            .iter()
            .map(|f| {
                let variant = f.variant();
                let id_lit = proc_macro2::Literal::u32_unsuffixed(f.id());
                quote! { #id_lit => Some(#enum_name::#variant) }
            })
            .chain(meter_fields.iter().map(|m| {
                let variant = snake_to_pascal(&m.ident);
                let id_lit = proc_macro2::Literal::u32_unsuffixed(m.id());
                quote! { #id_lit => Some(#enum_name::#variant) }
            }))
            .collect();

        quote! {
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
            #[repr(u32)]
            pub enum #enum_name {
                #(#variants,)*
            }

            impl #enum_name {
                pub const fn as_u32(self) -> u32 {
                    self as u32
                }

                pub fn from_u32(id: u32) -> Option<Self> {
                    match id {
                        #(#from_u32_arms,)*
                        _ => None,
                    }
                }
            }

            impl From<#enum_name> for u32 {
                fn from(id: #enum_name) -> u32 {
                    id as u32
                }
            }
        }
    } else {
        quote! {}
    };

    // Surface every `compile_error!` collected by `parse_param_attrs`
    // (unknown keys, wrong literal kinds, malformed `default = ...`).
    // Emitted alongside the impl rather than instead of it so the
    // diagnostics are precise; downstream type errors from a malformed
    // attribute aren't masked by a missing `Params` impl.
    let attr_errors: Vec<proc_macro2::TokenStream> = param_fields
        .iter()
        .flat_map(|f| f.attrs.errors.iter().cloned())
        .collect();

    // `#[persist]` codegen. The blob is a self-delimiting keyed list so
    // fields can be added / removed / reordered without breaking older
    // saves: a `u32` count, then per entry a `String` key and a
    // `u32`-length-prefixed value. Own `#[persist]` fields and `#[nested]`
    // sub-params share one list; a nested entry's value is that
    // sub-struct's own `serialize_persist` blob, so it recurses.
    let persist_write_stmts: Vec<proc_macro2::TokenStream> = persist_fields
        .iter()
        .map(|(ident, key, _)| {
            (
                key.clone(),
                quote! {
                    ::moose::core::custom_state::PersistField::persist_write(&self.#ident, &mut __buf);
                },
            )
        })
        .chain(nested_idents.iter().map(|ident| {
            (
                ident.to_string(),
                quote! {
                    __buf.extend_from_slice(&self.#ident.serialize_persist());
                },
            )
        }))
        .map(|(key, write)| {
            quote! {
                <::std::string::String as ::moose::core::custom_state::StateField>::write_field(
                    &#key.to_string(), &mut __buf,
                );
                let __start = __buf.len();
                __buf.extend_from_slice(&0u32.to_le_bytes());
                #write
                #[allow(clippy::cast_possible_truncation)]
                let __len = (__buf.len() - __start - 4) as u32;
                __buf[__start..__start + 4].copy_from_slice(&__len.to_le_bytes());
            }
        })
        .collect();

    let persist_seen_idents: Vec<syn::Ident> = persist_fields
        .iter()
        .map(|(ident, _, _)| syn::Ident::new(&format!("__seen_{}", ident.unraw()), ident.span()))
        .collect();
    let persist_read_arms: Vec<proc_macro2::TokenStream> = persist_fields
        .iter()
        .zip(&persist_seen_idents)
        .map(|((ident, key, missing), seen)| {
            let mark = missing.then(|| quote! { #seen = true; });
            quote! {
                #key => {
                    #mark
                    ::moose::core::custom_state::PersistField::persist_read(
                        &self.#ident,
                        &mut ::moose::core::custom_state::StateCursor::new(__value),
                    );
                }
            }
        })
        .chain(nested_idents.iter().map(|ident| {
            let key = ident.to_string();
            quote! {
                #key => self.#ident.load_persist(__value),
            }
        }))
        .collect();

    // `#[persist_missing]`: a non-empty blob without this key still
    // reaches the field (empty cursor) so it can migrate old documents.
    let persist_missing_stmts: Vec<proc_macro2::TokenStream> = persist_fields
        .iter()
        .zip(&persist_seen_idents)
        .filter(|((_, _, missing), _)| *missing)
        .map(|((ident, _, _), seen)| {
            quote! {
                if !__data.is_empty() && !#seen {
                    ::moose::core::custom_state::PersistField::persist_read(
                        &self.#ident,
                        &mut ::moose::core::custom_state::StateCursor::new(&[]),
                    );
                }
            }
        })
        .collect();

    let persist_missing_seen: Vec<&syn::Ident> = persist_fields
        .iter()
        .zip(&persist_seen_idents)
        .filter(|((_, _, missing), _)| *missing)
        .map(|(_, seen)| seen)
        .collect();

    #[allow(clippy::cast_possible_truncation)]
    let persist_count = (persist_fields.len() + nested_idents.len()) as u32;

    // Struct-level hooks (`#[params(...)]`).
    let pre_save = struct_attrs
        .pre_save
        .as_ref()
        .map(|m| quote! { self.#m(); });
    let post_load = struct_attrs
        .post_load
        .as_ref()
        .map(|m| quote! { self.#m(); });

    // Use precisely the persistence inventory above: new fields and nested
    // stores participate automatically, without a second handwritten catalog.
    let persist_fingerprint_stmts = persist_fields.iter().map(|(ident, key, _)| {
        quote! {
            __visitor.begin_field(#key);
            if let Some(__fingerprint) =
                ::moose::core::custom_state::PersistField::persist_fingerprint(&self.#ident)
            {
                __visitor.fingerprint(__fingerprint);
            } else {
                let mut __bytes = ::std::vec::Vec::new();
                ::moose::core::custom_state::PersistField::persist_write(&self.#ident, &mut __bytes);
                __visitor.serialized(&__bytes);
            }
            __visitor.end_field();
        }
    });
    let nested_fingerprint_stmts = nested_idents.iter().map(|ident| {
        let key = ident.to_string();
        quote! {
            __visitor.begin_field(#key);
            ::moose::params::Params::visit_persist_fingerprints(&self.#ident, __visitor);
            __visitor.end_field();
        }
    });
    let persist_fingerprint_impl = quote! {
        fn visit_persist_fingerprints(
            &self,
            __visitor: &mut dyn ::moose::params::PersistFingerprintVisitor,
        ) {
            #pre_save
            #(#persist_fingerprint_stmts)*
            #(#nested_fingerprint_stmts)*
        }
    };
    let persist_signal_fields = persist_fields.iter().map(|(ident, _, _)| {
        quote! {
            ::moose::core::custom_state::PersistField::set_persist_change_signal(
                &self.#ident, __signal.clone());
        }
    });
    let persist_changed_impl = struct_attrs.persist_changed.as_ref().map(|method| {
        quote! {
            fn wants_persist_notifications(&self) -> bool { true }
            fn take_persist_change(&self) -> Option<(u32, f64)> { self.#method() }
        }
    });
    let persist_signal_impl = quote! {
        fn set_persist_change_signal(
            &self, __signal: ::std::sync::Weak<::moose::params::PersistChangeSignal>,
        ) {
            #(#persist_signal_fields)*
            #(::moose::params::Params::set_persist_change_signal(
                &self.#nested_idents, __signal.clone());)*
        }
        #persist_changed_impl
    };
    let prepare_write_fields = persist_fields.iter().map(|(ident, key, _)| {
        quote! {
            __fields.push((#key,
                ::moose::core::custom_state::PersistField::prepare_persist_write(&self.#ident)?));
        }
    });
    let prepare_write_nested = nested_idents.iter().map(|ident| {
        let key = ident.to_string();
        quote! {
            __fields.push((#key,
                ::moose::params::Params::prepare_persist_write(&self.#ident)?));
        }
    });
    let prepare_write_body = if persist_count == 0 {
        // A store without fields has historically serialized no count word.
        quote! { Ok(Box::new(::std::vec::Vec::<u8>::new())) }
    } else {
        quote! {
            let mut __fields: ::std::vec::Vec<(&'static str,
                Box<dyn ::moose::params::PreparedPersistWrite>)> = ::std::vec::Vec::new();
            #(#prepare_write_fields)*
            #(#prepare_write_nested)*
            Ok(Box::new(::moose::params::KeyedPersistWrite::new(__fields)?))
        }
    };
    let prepare_write_impl = quote! {
        fn prepare_persist_write(&self)
            -> ::std::io::Result<Box<dyn ::moose::params::PreparedPersistWrite>>
        {
            #pre_save
            #prepare_write_body
        }
    };
    let validate_impl = struct_attrs.validate_persist.as_ref().map(|m| {
        quote! {
            fn validate_persist(data: &[u8]) -> bool where Self: Sized { Self::#m(data) }
        }
    });
    let restore_impl = struct_attrs.restore_state.as_ref().map(|m| {
        quote! {
            fn restore_state(&self, values: &[(u32, f64)], persist: &[u8]) {
                self.#m(values, persist);
            }
        }
    });
    let prepare_stream_impl = struct_attrs.prepare_persist_stream.as_ref().map(|m| {
        quote! {
            fn prepare_persist_stream(read: &mut dyn ::std::io::Read, len: usize)
                -> Option<::std::io::Result<(Vec<u8>, ::moose::params::PreparedPersist<Self>)>>
            {
                Self::#m(read, len)
            }
        }
    });
    // Without a struct-level hook, presentation, revision and function come
    // from the `#[nested]` groups (a runtime `DynamicParams` answers there).
    let presentation_impl = match struct_attrs.presentation.as_ref() {
        Some(m) => Some(quote! {
            fn parameter_presentation(&self, id: u32)
                -> ::core::option::Option<::moose::params::ParameterPresentation>
            {
                self.#m(id)
            }
        }),
        None if !nested_idents.is_empty() => Some(quote! {
            fn parameter_presentation(&self, id: u32)
                -> ::core::option::Option<::moose::params::ParameterPresentation>
            {
                #(if let Some(p) = ::moose::params::Params::parameter_presentation(&self.#nested_idents, id) {
                    return Some(p);
                })*
                None
            }
        }),
        None => None,
    };
    let presentation_revision_impl = match struct_attrs.presentation_revision.as_ref() {
        Some(m) => Some(quote! {
            fn parameter_presentation_revision(&self) -> u64 { self.#m() }
        }),
        None if !nested_idents.is_empty() => Some(quote! {
            fn parameter_presentation_revision(&self) -> u64 {
                0u64 #(.wrapping_add(::moose::params::Params::parameter_presentation_revision(&self.#nested_idents)))*
            }
        }),
        None => None,
    };
    let function_impl = if let Some(m) = struct_attrs.function.as_ref() {
        quote! {
            fn parameter_function(&self, id: u32) -> Option<::moose::params::ParamFunction> { self.#m(id) }
        }
    } else {
        quote! {
            fn parameter_function(&self, id: u32) -> Option<::moose::params::ParamFunction> {
                #(if let Some(f) = ::moose::params::Params::parameter_function(&self.#nested_idents, id) {
                    return Some(f);
                })*
                let _ = id;
                None
            }
        }
    };
    let value_list_idents: Vec<_> = param_fields
        .iter()
        .filter(|f| f.kind == ParamKind::ValueList)
        .map(ParamField::access)
        .collect();
    let value_list_label_arms: Vec<_> = param_fields
        .iter()
        .filter_map(|f| {
            let ident = f.access();
            match f.kind {
                ParamKind::ValueList => Some(quote! {
                    x if x == self.#ident.id() => Some(self.#ident.labels()),
                }),
                ParamKind::Enum => {
                    let enum_ty = f
                        .enum_type
                        .as_ref()
                        .expect("ParamKind::Enum field must have enum_type populated");
                    Some(quote! {
                        x if x == self.#ident.id() => Some(
                            <#enum_ty as ::moose::params::ParamEnum>::variant_names()
                                .iter()
                                .map(|s| (*s).to_owned())
                                .collect(),
                        ),
                    })
                }
                _ => None,
            }
        })
        .collect();
    let value_list_labels_impl = if value_list_label_arms.is_empty() && nested_idents.is_empty() {
        quote! {}
    } else {
        quote! {
            fn value_list_labels(&self, id: u32) -> Option<Vec<String>> {
                match id {
                    #(#value_list_label_arms)*
                    _ => {
                        #(if let Some(labels) = ::moose::params::Params::value_list_labels(&self.#nested_idents, id) { return Some(labels); })*
                        None
                    }
                }
            }
        }
    };
    let own_string_id_pushes: Vec<_> = param_fields
        .iter()
        .map(|f| {
            let ident = f.access();
            let key = f.string_key();
            quote! { __out.push((self.#ident.id(), #key.to_owned())); }
        })
        .collect();
    let nested_string_id_pushes: Vec<_> = nested_fields
        .iter()
        .map(|n| {
            let ident = &n.ident;
            let prefix = n.ident.to_string();
            quote! {
                for (__id, __sub) in ::moose::params::Params::param_string_ids(&self.#ident) {
                    __out.push((__id, format!("{}.{}", #prefix, __sub)));
                }
            }
        })
        .collect();
    let param_string_ids_impl = quote! {
        fn param_string_ids(&self) -> Vec<(u32, String)> {
            #[allow(unused_mut)]
            let mut __out = Vec::new();
            #(#own_string_id_pushes)*
            #(#nested_string_id_pushes)*
            __out
        }
    };
    let catalog_impl = quote! {
        fn set_catalog_signal(&self, __signal: ::std::sync::Weak<::moose::params::CatalogSignal>) {
            #(self.#value_list_idents.set_catalog_signal(__signal.clone());)*
            #(::moose::params::Params::set_catalog_signal(&self.#nested_idents, __signal.clone());)*
            let _ = __signal;
        }
        fn take_catalog_changes(&self) -> ::moose::params::CatalogChanges {
            #[allow(unused_mut)]
            let mut __changes = ::moose::params::CatalogChanges::default();
            #(__changes.merge(self.#value_list_idents.take_catalog_changes());)*
            #(__changes.merge(::moose::params::Params::take_catalog_changes(&self.#nested_idents));)*
            __changes
        }
    };

    let presentation_change_impl = struct_attrs.presentation_change.as_ref().map(|m| {
        quote! {
            fn parameter_affects_presentation(&self, id: u32) -> bool { self.#m(id) }
        }
    });

    let value_format_override = struct_attrs.value_format.as_ref().map(|method| {
        quote! {
            if let Some(text) = self.#method(id, value) { return Some(text); }
        }
    });
    let value_parse_override = struct_attrs.value_parse.as_ref().map(|method| {
        quote! {
            match self.#method(id, text) {
                Ok(Some(value)) => return value.is_finite().then_some(value),
                Ok(None) => {},
                Err(()) => return None,
            }
        }
    });

    // Nothing to carry: leave the empty-`Vec` trait default so a plugin
    // without persisted config adds no bytes to its saved state. A
    // `pre_save` / `post_load` / `validate_persist` hook still needs the
    // methods so the hook runs.
    let persist_impl = if persist_fields.is_empty() && nested_idents.is_empty() {
        let save = pre_save.as_ref().map(|pre| {
            quote! {
                fn serialize_persist(&self) -> ::std::vec::Vec<u8> {
                    #pre
                    ::std::vec::Vec::new()
                }
            }
        });
        let load = (post_load.is_some() || validate_impl.is_some()).then(|| {
            quote! {
                fn load_persist(&self, __data: &[u8]) {
                    if !<Self as ::moose::params::Params>::validate_persist(__data) { return; }
                    #post_load
                }
            }
        });
        quote! { #save #load }
    } else {
        quote! {
        fn serialize_persist(&self) -> ::std::vec::Vec<u8> {
            #pre_save
            let mut __buf: ::std::vec::Vec<u8> = ::std::vec::Vec::new();
            <u32 as ::moose::core::custom_state::StateField>::write_field(&#persist_count, &mut __buf);
            #(#persist_write_stmts)*
            __buf
        }

        fn load_persist(&self, __data: &[u8]) {
            if !<Self as ::moose::params::Params>::validate_persist(__data) { return; }
            #(let mut #persist_missing_seen = false;)*
            // Early exits on a truncated blob still run the
            // missing-field migrations and `post_load` below.
            '__read: {
                let mut __cursor = ::moose::core::custom_state::StateCursor::new(__data);
                let ::core::option::Option::Some(__count) =
                    <u32 as ::moose::core::custom_state::StateField>::read_field(&mut __cursor)
                else {
                    break '__read;
                };
                for _ in 0..__count {
                    let ::core::option::Option::Some(__key) =
                        <::std::string::String as ::moose::core::custom_state::StateField>::read_field(&mut __cursor)
                    else {
                        break '__read;
                    };
                    let ::core::option::Option::Some(__len_bytes) = __cursor.read_bytes(4) else {
                        break '__read;
                    };
                    let __len = u32::from_le_bytes(
                        __len_bytes.try_into().expect("read_bytes(4) yields 4 bytes"),
                    ) as usize;
                    let ::core::option::Option::Some(__value) = __cursor.read_bytes(__len) else {
                        break '__read;
                    };
                    match __key.as_str() {
                        #(#persist_read_arms)*
                        _ => {}
                    }
                }
            }
            #(#persist_missing_stmts)*
            #post_load
        }
        }
    };

    // Projection dispatch is emitted only for opted-in stores. Declared IDs keep
    // their original match arms; alias storage never enters state collection.
    let mut projected_setters = quote! {};
    let (
        count_expr,
        infos_expr,
        append_infos_impl,
        get_plain_fallthrough,
        get_normalized_fallthrough,
        set_plain_fallthrough,
        set_normalized_fallthrough,
        format_fallthrough,
        parse_value_impl,
    ) = if let Some(provider) = &struct_attrs.projection {
        let api = quote! { <#provider as ::moose::params::ParamProjection<Self>> };
        projected_setters = quote! {
            fn param_catalog(&self) -> ::std::sync::Arc<::moose::params::ParamCatalog> {
                static CATALOG: ::std::sync::LazyLock<::std::sync::Arc<::moose::params::ParamCatalog>> =
                    ::std::sync::LazyLock::new(|| ::std::sync::Arc::new(::moose::params::ParamCatalog::new(
                        <#struct_name as ::moose::params::Params>::param_infos_static())));
                ::std::sync::Arc::clone(&CATALOG)
            }
            fn set_normalized_returning_plain(&self, id: u32, value: f64) -> f64 {
                if let Some(result) = #api::set_normalized(self, id, value) { return result.plain; }
                <Self as ::moose::params::Params>::set_normalized(self, id, value);
                <Self as ::moose::params::Params>::get_plain(self, id).unwrap_or(0.0)
            }
            fn set_normalized_returning_normalized(&self, id: u32, value: f64) -> f64 {
                if let Some(result) = #api::set_normalized(self, id, value) { return result.normalized; }
                <Self as ::moose::params::Params>::set_normalized(self, id, value);
                <Self as ::moose::params::Params>::get_normalized(self, id).unwrap_or(0.0)
            }
        };
        (
            quote! { (#count_expr) + #api::param_infos().len() },
            quote! {
                let mut infos = { #infos_expr };
                ::moose::params::append_projected_infos::<Self, #provider>(&mut infos);
                infos
            },
            quote! {
                fn append_param_infos(&self, into: &mut Vec<::moose::params::ParamInfo>) {
                    #(into.push(#own_infos);)*
                    #(self.#nested_idents.append_param_infos(into);)*
                    ::moose::params::append_projected_infos::<Self, #provider>(into);
                }
            },
            quote! { _ => { (match id { #get_plain_fallthrough }).or_else(|| #api::get_plain(self, id)) } },
            quote! { _ => { (match id { #get_normalized_fallthrough }).or_else(|| #api::get_normalized(self, id)) } },
            quote! { _ => {
                if #api::set_plain(self, id, value).is_none() { match id { #set_plain_fallthrough } }
            } },
            quote! { _ => {
                if #api::set_normalized(self, id, value).is_none() { match id { #set_normalized_fallthrough } }
            } },
            quote! { _ => { (match id { #format_fallthrough }).or_else(|| #api::format_value(self, id, value)) } },
            quote! { (#parse_value_impl).or_else(|| #api::parse_value(self, id, text).filter(|value| value.is_finite())) },
        )
    } else {
        (
            count_expr,
            infos_expr,
            append_infos_impl,
            get_plain_fallthrough,
            get_normalized_fallthrough,
            set_plain_fallthrough,
            set_normalized_fallthrough,
            format_fallthrough,
            parse_value_impl,
        )
    };

    let projection_prefix = |method: &str| {
        struct_attrs.projection.as_ref().map(|provider| {
            let method = syn::Ident::new(method, struct_name.span());
            quote! {
                if id > ::moose::params::AUTO_PARAM_ID_MASK {
                    if let Some(value) = <#provider as ::moose::params::ParamProjection<Self>>::#method(self, id) {
                        return Some(value);
                    }
                }
            }
        })
    };
    let projected_get_plain = projection_prefix("get_plain");
    let projected_get_normalized = projection_prefix("get_normalized");
    let projection_write_prefix = |method: &str| {
        struct_attrs.projection.as_ref().map(|provider| {
            let method = syn::Ident::new(method, struct_name.span());
            quote! {
                if id > ::moose::params::AUTO_PARAM_ID_MASK
                    && <#provider as ::moose::params::ParamProjection<Self>>::#method(self, id, value).is_some() {
                    return;
                }
            }
        })
    };
    let projected_write_plain = projection_write_prefix("set_plain");
    let projected_write_normalized = projection_write_prefix("set_normalized");

    let expanded = quote! {
        #(#attr_errors)*

        #new_impl

        #offset_ids_impl

        #param_id_enum

        impl ::moose::params::__private::Sealed for #struct_name {}

        impl ::moose::params::Params for #struct_name {
            #parameter_epoch_impl
            #projected_setters
            fn param_infos(&self) -> Vec<::moose::params::ParamInfo> {
                #infos_expr
            }

            #append_infos_impl

            #param_infos_static_impl

            #presentation_impl
            #presentation_revision_impl
            #presentation_change_impl
            #function_impl
            #param_string_ids_impl
            #value_list_labels_impl
            #catalog_impl
            #validate_impl
            #restore_impl
            #prepare_stream_impl

            fn count(&self) -> usize {
                #count_expr
            }

            fn meter_ids(&self) -> Vec<u32> {
                #meter_ids_expr
            }

            fn get_normalized(&self, id: u32) -> Option<f64> {
                #projected_get_normalized
                match id {
                    #(#get_normalized_arms)*
                    #get_normalized_fallthrough
                }
            }

            fn set_normalized(&self, id: u32, value: f64) {
                #projected_write_normalized
                match id {
                    #(#set_normalized_arms)*
                    #set_normalized_fallthrough
                }
            }

            fn get_plain(&self, id: u32) -> Option<f64> {
                #projected_get_plain
                match id {
                    #(#get_plain_arms)*
                    #get_plain_fallthrough
                }
            }

            fn get_plain_by_index(&self, index: usize, id: u32) -> Option<f64> {
                #projected_get_plain
                match index {
                    #(#get_plain_by_index_arms)*
                    #get_plain_by_index_fallthrough
                }
            }

            fn set_plain(&self, id: u32, value: f64) {
                #projected_write_plain
                match id {
                    #(#set_plain_arms)*
                    #set_plain_fallthrough
                }
            }

            fn set_plain_by_index(&self, index: usize, id: u32, value: f64) {
                #projected_write_plain
                match index {
                    #(#set_plain_by_index_arms)*
                    #set_plain_by_index_fallthrough
                }
            }

            fn format_value(&self, id: u32, value: f64) -> Option<String> {
                #value_format_override
                match id {
                    #(#format_value_arms)*
                    #format_fallthrough
                }
            }

            fn format_value_by_index(&self, index: usize, id: u32, value: f64) -> Option<String> {
                #value_format_override
                match index {
                    #(#format_value_by_index_arms)*
                    #format_by_index_fallthrough
                }
            }

            fn parse_value(&self, id: u32, text: &str) -> Option<f64> {
                #value_parse_override
                #parse_value_impl
            }

            fn parse_value_by_index(&self, index: usize, id: u32, text: &str) -> Option<f64> {
                #value_parse_override
                match index {
                    #(#parse_value_by_index_arms)*
                    #parse_by_index_fallthrough
                }
            }

            fn set_modulation(&self, id: u32, amount: f64) {
                match id {
                    #(#modulation_arms)*
                    _ => {
                        #(self.#nested_idents.set_modulation(id, amount);)*
                    }
                }
            }

            fn clear_modulations(&self) {
                #(#clear_modulation_stmts)*
                #(self.#nested_idents.clear_modulations();)*
            }

            fn snap_smoothers(&self) {
                #(#snap_stmts)*
                #(self.#nested_idents.snap_smoothers();)*
            }

            fn set_sample_rate(&self, sample_rate: f64) {
                #(#sr_stmts)*
                #(self.#nested_idents.set_sample_rate(sample_rate);)*
            }

            fn collect_values(&self) -> (Vec<u32>, Vec<f64>) {
                #pre_save
                let mut ids: Vec<u32> = vec![#(#collect_ids),*];
                let mut values: Vec<f64> = ids
                    .iter()
                    .map(|id| self.get_plain(*id).expect("id was emitted by #[derive(Params)] and so must resolve"))
                    .collect();
                #({
                    let (nids, nvals) = self.#nested_idents.collect_values();
                    ids.extend(nids);
                    values.extend(nvals);
                })*
                (ids, values)
            }

            fn restore_values(&self, values: &[(u32, f64)]) {
                for (id, value) in values {
                    self.set_plain(*id, *value);
                }
            }

            #persist_impl
            #persist_fingerprint_impl
            #persist_signal_impl
            #prepare_write_impl
        }
    };

    expanded.into()
}

/// Convert a `snake_case` field name to a `PascalCase` enum variant ident.
///
/// Handles the awkward edge cases the original `split('_').map(...)` form
/// would silently produce nonsense for:
///
/// - **Raw idents** (`r#type`): the `r#` prefix is stripped via
///   `Ident::unraw()` rather than string-prefix matching, so callers
///   don't need to know whether `syn` rendered the ident raw.
/// - **Leading digit after underscore strip** (`_3band` → `"3band"`):
///   `split('_')` drops the leading `_`, leaving a fragment that can't
///   start an enum variant. The guard prepends `_` to produce
///   `"_3band"`. (Pure `r#3band` is unreachable - `Ident::new_raw`
///   rejects digit-first idents - but `_3band` is a legal Rust ident
///   and is the case the test actually exercises.)
/// - **All-non-alphanumeric** (`__`, `___`, `_`): `Ident::new("", span)` would
///   panic and single `_` is a reserved token that cannot name an enum
///   variant, so multi-underscore names are preserved verbatim (`__`, `___`)
///   and single `_` maps to `_Field`.
pub(crate) fn snake_to_pascal(ident: &syn::Ident) -> syn::Ident {
    let raw = ident.unraw().to_string();
    let mut pascal: String = raw
        .split('_')
        .filter(|w| !w.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if pascal.is_empty() {
        // `_` alone is a reserved token and cannot name an enum variant;
        // preserve multi-underscore names (`__`, `___`) which are legal
        // identifiers, and map single `_` to `_Field`.
        if raw.len() > 1 {
            pascal = raw;
        } else {
            pascal.push_str("_Field");
        }
    } else if pascal.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        pascal.insert(0, '_');
    }
    if pascal == "Self" {
        pascal.push('_');
    }
    syn::Ident::new(&pascal, ident.span())
}

/// The first clash among a struct's own stable string IDs, or between an
/// own ID and a `#[nested]` field's `prefix.` namespace.
fn string_key_conflict(keys: &[String], nested: &[String]) -> Option<String> {
    let mut seen = HashSet::new();
    for key in keys {
        if !seen.insert(key.as_str()) {
            return Some(format!(
                "duplicate parameter string ID `{key}`: give one field a distinct \
                 `#[param(key = \"...\")]`"
            ));
        }
        if let Some(prefix) = nested.iter().find(|prefix| {
            key.strip_prefix(prefix.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
        }) {
            return Some(format!(
                "parameter string ID `{key}` is inside the `{prefix}.` namespace of the \
                 `#[nested]` field `{prefix}`"
            ));
        }
    }
    None
}

#[cfg(test)]
mod string_key_tests {
    use super::string_key_conflict;

    fn own(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn string_key_conflicts_are_compile_errors() {
        assert_eq!(
            string_key_conflict(&own(&["gain", "mix"]), &own(&["osc1"])),
            None
        );
        assert!(
            string_key_conflict(&own(&["gain", "gain"]), &[])
                .is_some_and(|m| m.contains("duplicate parameter string ID `gain`"))
        );
        assert!(
            string_key_conflict(&own(&["osc1.cutoff"]), &own(&["osc1"]))
                .is_some_and(|m| m.contains("`osc1.` namespace"))
        );
        assert_eq!(
            string_key_conflict(&own(&["osc10.cutoff", "osc1"]), &own(&["osc1"])),
            None,
            "only a `prefix.` match is inside the namespace"
        );
    }
}

#[cfg(test)]
mod snake_to_pascal_tests {
    use super::*;
    use proc_macro2::Span;

    fn convert(s: &str) -> String {
        // Keywords need raw-ident syntax to round-trip through `syn::Ident`.
        // Idents starting with a digit aren't constructible at all (not
        // even as `Ident::new_raw`); a leading-`_` ident like `_3band`
        // *is* valid and exercises the same branch after `split('_')`
        // strips the underscore.
        let id = if matches!(s, "type" | "fn" | "let" | "match") {
            syn::Ident::new_raw(s, Span::call_site())
        } else {
            syn::Ident::new(s, Span::call_site())
        };
        snake_to_pascal(&id).to_string()
    }

    #[test]
    fn self_keyword_is_escaped() {
        assert_eq!(convert("self_"), "Self_");
    }

    #[test]
    fn ordinary_snake_case() {
        assert_eq!(convert("gain"), "Gain");
        assert_eq!(convert("low_pass"), "LowPass");
        assert_eq!(convert("multi_word_field"), "MultiWordField");
    }

    #[test]
    fn raw_keyword_ident() {
        assert_eq!(convert("type"), "Type");
    }

    #[test]
    fn leading_digit_prepends_underscore() {
        // `_3band` is a legal Rust ident; `split('_')` strips the leading
        // underscore and the surviving fragment "3band" starts with a
        // digit, which can't begin an enum variant. Guard prepends `_` so
        // the output `_3band` is a valid variant ident.
        assert_eq!(convert("_3band"), "_3band");
    }

    #[test]
    fn all_underscores_produces_valid_variant_ident() {
        // `___` must not collapse to reserved token `_`.
        assert_eq!(convert("___"), "___");
        assert_eq!(convert("__"), "__");
    }
}

/// Per-id match arms (`x if x == self.field.id() => body,`).
fn id_arms(bodies: &[(TokenStream2, TokenStream2)]) -> Vec<TokenStream2> {
    bodies
        .iter()
        .map(|(ident, body)| quote! { x if x == self.#ident.id() => #body, })
        .collect()
}

/// D6 dense-index match arms (`N if self.field.id() == id => body,`). The
/// id guard rejects a stale or foreign index, so the caller's fallthrough
/// resolves it by id instead of touching the wrong field.
fn index_arms(bodies: &[(TokenStream2, TokenStream2)]) -> Vec<TokenStream2> {
    bodies
        .iter()
        .enumerate()
        .map(|(idx, (ident, body))| quote! { #idx if self.#ident.id() == id => #body, })
        .collect()
}
