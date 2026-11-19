//! Parse `#[derive(Params)]` fields and their `#[param]` attributes.

use crate::params::snake_to_pascal;

use moose_params::AUTO_PARAM_ID_MASK;
use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, quote};
use syn::{Expr, Fields, Lit, Type, TypePath, UnOp};

/// Recognized parameter field types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamKind {
    Float,
    Bool,
    Int,
    Enum,
}

/// A parsed parameter field from the input struct.
pub(crate) struct ParamField {
    pub(crate) ident: syn::Ident,
    pub(crate) kind: ParamKind,
    pub(crate) attrs: ParamAttrs,
    /// For `EnumParam<T>`, the inner type `T`.
    pub(crate) enum_type: Option<syn::Type>,
    /// For an element of an array field (`[FloatParam; N]`), its index:
    /// the array expands into one `ParamField` per element, each with
    /// its own id and name, reached as `self.field[index]`.
    pub(crate) index: Option<usize>,
}

impl ParamField {
    /// ID that the auto-assignment block at the top of `derive_params`
    /// has guaranteed is populated. Calling this before the auto-assign
    /// loop runs is a logic error - the `expect` message names the
    /// invariant rather than just panicking with `unwrap`'s opaque
    /// `called Option::unwrap on a None`.
    pub(crate) fn id(&self) -> u32 {
        self.attrs
            .id
            .expect("ParamField::id called before the auto-assignment block ran")
    }

    /// The param as a place below `self`: `field`, or `field[i]` for an
    /// array element.
    pub(crate) fn access(&self) -> TokenStream2 {
        let ident = &self.ident;
        self.index.map_or_else(
            || quote! { #ident },
            |i| {
                let i = proc_macro2::Literal::usize_unsuffixed(i);
                quote! { #ident[#i] }
            },
        )
    }

    /// The param's path as text (`field`, `field[i]`): its hashed id and
    /// its entry in the param-index sidecar.
    pub(crate) fn path(&self) -> String {
        match self.index {
            Some(i) => format!("{}[{i}]", self.ident),
            None => self.ident.to_string(),
        }
    }

    /// Its `ParamId` variant: `Field`, or `Field{i}` for an array element.
    pub(crate) fn variant(&self) -> syn::Ident {
        let base = snake_to_pascal(&self.ident);
        match self.index {
            Some(i) => syn::Ident::new(&format!("{base}{i}"), base.span()),
            None => base,
        }
    }
}

/// A nested Params field (delegates to inner struct).
pub(crate) struct NestedField {
    pub(crate) ident: syn::Ident,
    /// Field type, retained so the derive can call associated
    /// functions on it without an instance - specifically
    /// `Params::param_infos_static` for the registration-time
    /// "no temp plugin" path.
    pub(crate) ty: syn::Type,
    /// Explicit base from `#[nested(base = N)]`, the global id of this
    /// group's local id 0. `None` auto-assigns the base by packing the
    /// group right after the preceding params (own params, then each
    /// earlier nested group's span). Pin it for wire stability the same
    /// way you pin a param `id`.
    pub(crate) base: Option<u32>,
}

/// A meter slot field.
pub(crate) struct MeterField {
    pub(crate) ident: syn::Ident,
    pub(crate) id: Option<u32>,
}

impl MeterField {
    /// ID that the auto-assignment block at the top of `derive_params`
    /// has guaranteed is populated. Same invariant as
    /// [`ParamField::id`].
    pub(crate) fn id(&self) -> u32 {
        self.id
            .expect("MeterField::id called before the auto-assignment block ran")
    }
}

/// A resolved `default = ...` expression plus the tokens to emit
/// for it. `value` drives the derive's compile-time range / shape
/// checks; `path_tokens` carries the original `std::f64::consts::*`
/// path through to the generated `ParamInfo` so clippy lints like
/// `approx_constant` see the path, not the resolved literal. For
/// numeric literals `path_tokens` stays `None` and the f64 value
/// gets quoted in the usual way.
#[derive(Clone)]
pub(crate) struct DefaultExpr {
    pub(crate) value: f64,
    pub(crate) path_tokens: Option<TokenStream2>,
}

/// Parsed `#[param(...)]` attributes.
#[derive(Clone, Default)]
pub(crate) struct ParamAttrs {
    pub(crate) id: Option<u32>,
    pub(crate) name: Option<String>,
    pub(crate) short_name: Option<String>,
    pub(crate) group: Option<String>,
    pub(crate) range: Option<String>,
    pub(crate) default: Option<DefaultExpr>,
    pub(crate) unit: Option<String>,
    pub(crate) flags: Option<String>,
    /// Set by `#[param(chunk = false)]` on parameters too expensive
    /// to retarget mid-block (FFT sizes, oversampling factors,
    /// lookahead spans). `None` means "use the default" (`true`).
    /// Drives the `ParamFlags::CHUNKED` bit in
    /// `gen_param_info_literal`. See the
    /// `parameter-dependent-chunking.md` design doc.
    pub(crate) chunk: Option<bool>,
    /// Default host MIDI-learn binding source, set by `midi_cc = N`
    /// or `midi_source = "pitchbend" | "pressure" | "program"` (at
    /// most one). Baked onto `ParamInfo::midi_map`.
    pub(crate) midi_map: Option<MidiBindKind>,
    /// Channel scope for `midi_map`, stored as the wire channel
    /// `0..=15` (the attribute is the user-facing `1..=16`).
    pub(crate) midi_channel: Option<u8>,
    pub(crate) smooth: Option<String>,
    pub(crate) format_fn: Option<String>,
    pub(crate) parse_fn: Option<String>,
    /// Compile-error tokens collected during parsing - emitted by
    /// the derive output so unknown keys and unexpected literal
    /// kinds surface at compile time instead of as silent default
    /// values.
    pub(crate) errors: Vec<proc_macro2::TokenStream>,
}

/// Derive-side mirror of `moose_params::MidiSource`, used to validate
/// bindings across params and emit the `ParamInfo::midi_map` tokens.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MidiBindKind {
    Cc(u8),
    PitchBend,
    ChannelPressure,
    ProgramChange,
}

pub(crate) const MIDI_BOTH_SET: &str = "set only one of `midi_cc` / `midi_source` on a parameter";

impl MidiBindKind {
    /// The `Some(::moose::params::MidiSource::…)` tokens for a binding,
    /// or `None`. CC values are emitted unsuffixed (`Cc(74)`) to keep
    /// the macro output rust-analyzer-friendly.
    pub(crate) fn to_tokens(self) -> proc_macro2::TokenStream {
        match self {
            MidiBindKind::Cc(n) => {
                let lit = proc_macro2::Literal::u8_unsuffixed(n);
                quote! { ::moose::params::MidiSource::Cc(#lit) }
            }
            MidiBindKind::PitchBend => quote! { ::moose::params::MidiSource::PitchBend },
            MidiBindKind::ChannelPressure => {
                quote! { ::moose::params::MidiSource::ChannelPressure }
            }
            MidiBindKind::ProgramChange => quote! { ::moose::params::MidiSource::ProgramChange },
        }
    }
}

pub(crate) fn type_last_segment(ty: &Type) -> Option<String> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    path.segments.last().map(|seg| seg.ident.to_string())
}

/// Extract the generic type argument from `EnumParam<T>`.
pub(crate) fn extract_enum_type_arg(ty: &Type) -> Option<syn::Type> {
    if let Type::Path(TypePath { path, .. }) = ty {
        let seg = path.segments.last()?;
        if seg.ident == "EnumParam"
            && let syn::PathArguments::AngleBracketed(args) = &seg.arguments
            && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
        {
            return Some(inner.clone());
        }
    }
    None
}

pub(crate) fn classify_param_type(ty: &Type) -> Option<ParamKind> {
    let name = type_last_segment(ty)?;
    match name.as_str() {
        "FloatParam" => Some(ParamKind::Float),
        "BoolParam" => Some(ParamKind::Bool),
        "IntParam" => Some(ParamKind::Int),
        "EnumParam" => Some(ParamKind::Enum),
        _ => None,
    }
}

/// Parse the `midi_cc` / `midi_source` / `midi_channel` keys. Split out
/// of [`parse_param_attrs`]'s loop to keep it under the line limit;
/// errors are pushed onto `attrs.errors` like the other arms.
pub(crate) fn parse_midi_attr(
    key: &str,
    meta: &syn::meta::ParseNestedMeta,
    attrs: &mut ParamAttrs,
) -> syn::Result<()> {
    let push = |attrs: &mut ParamAttrs, e: syn::Error| attrs.errors.push(e.to_compile_error());
    let value: Lit = meta.value()?.parse()?;
    match (key, value) {
        ("midi_cc", Lit::Int(lit)) => match lit.base10_parse::<u16>() {
            Ok(cc) if cc <= 127 => {
                let cc = u8::try_from(cc).expect("checked <= 127");
                if attrs.midi_map.is_some() {
                    push(attrs, meta.error(MIDI_BOTH_SET));
                } else {
                    attrs.midi_map = Some(MidiBindKind::Cc(cc));
                }
            }
            _ => push(
                attrs,
                syn::Error::new_spanned(lit, "`#[param(midi_cc = …)]` must be 0..=127"),
            ),
        },
        ("midi_source", Lit::Str(s)) => {
            let kind = match s.value().as_str() {
                "pitchbend" => Some(MidiBindKind::PitchBend),
                "pressure" => Some(MidiBindKind::ChannelPressure),
                "program" => Some(MidiBindKind::ProgramChange),
                _ => None,
            };
            match kind {
                _ if attrs.midi_map.is_some() => push(attrs, meta.error(MIDI_BOTH_SET)),
                Some(k) => attrs.midi_map = Some(k),
                None => push(
                    attrs,
                    syn::Error::new_spanned(
                        s,
                        "`#[param(midi_source = …)]` expects \"pitchbend\", \"pressure\", \
                         or \"program\"",
                    ),
                ),
            }
        }
        ("midi_channel", Lit::Int(lit)) => match lit.base10_parse::<u16>() {
            Ok(ch) if (1..=16).contains(&ch) => {
                attrs.midi_channel = Some(u8::try_from(ch - 1).expect("checked 1..=16"));
            }
            _ => push(
                attrs,
                syn::Error::new_spanned(lit, "`#[param(midi_channel = …)]` must be 1..=16"),
            ),
        },
        ("midi_source", other) => push(
            attrs,
            syn::Error::new_spanned(
                other,
                "`#[param(midi_source = …)]` expects a string literal",
            ),
        ),
        (k, other) => push(
            attrs,
            syn::Error::new_spanned(
                other,
                format!("`#[param({k} = …)]` expects an integer literal"),
            ),
        ),
    }
    Ok(())
}

/// Parse `#[param(...)]` attributes from a field. Errors carried in
/// `attrs.errors` instead of bubbling out so the caller can keep
/// collecting (each malformed attribute should produce a separate
/// `compile_error!` rather than the first one short-circuiting).
pub(crate) fn parse_param_attrs(field: &syn::Field) -> ParamAttrs {
    let mut attrs = ParamAttrs::default();
    // Helper: turn a `syn::Error` into a `compile_error!` token stream
    // and stash it. Used both by the explicit "unknown key" / "wrong
    // literal kind" arms and by `parse_nested_meta`'s own bubbled
    // errors below.
    let push_err = |attrs: &mut ParamAttrs, e: syn::Error| {
        attrs.errors.push(e.to_compile_error());
    };
    for attr in &field.attrs {
        if !attr.path().is_ident("param") {
            continue;
        }
        // `parse_nested_meta`'s closure can only return one error per
        // call (it short-circuits the *current* attribute group on
        // first Err), so route per-key errors through `attrs.errors`
        // instead - each malformed key generates a `compile_error!`
        // and parsing continues.
        let parse_result = attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map_or_else(String::new, std::string::ToString::to_string);
            // Two-step pattern for the string-typed keys: parse the
            // literal first, then either assign or stash a
            // compile_error. Avoids needing `&mut attrs` aliased with
            // `&mut attrs.<field>` inside a closure.
            let take_str_into = |slot: &mut Option<String>,
                                 errors: &mut Vec<proc_macro2::TokenStream>,
                                 key_name: &str|
             -> syn::Result<()> {
                let value: Lit = meta.value()?.parse()?;
                match value {
                    Lit::Str(lit) => {
                        *slot = Some(lit.value());
                    }
                    other => {
                        errors.push(
                            syn::Error::new_spanned(
                                other,
                                format!("`#[param({key_name} = ...)]` expects a string literal"),
                            )
                            .to_compile_error(),
                        );
                    }
                }
                Ok(())
            };
            match key.as_str() {
                "id" => {
                    let value: Lit = meta.value()?.parse()?;
                    match value {
                        Lit::Int(lit) => attrs.id = Some(lit.base10_parse()?),
                        other => push_err(
                            &mut attrs,
                            syn::Error::new_spanned(
                                other,
                                "`#[param(id = ...)]` expects an integer literal",
                            ),
                        ),
                    }
                }
                "name" => take_str_into(&mut attrs.name, &mut attrs.errors, "name")?,
                "short_name" => {
                    take_str_into(&mut attrs.short_name, &mut attrs.errors, "short_name")?;
                }
                "group" => take_str_into(&mut attrs.group, &mut attrs.errors, "group")?,
                "range" => take_str_into(&mut attrs.range, &mut attrs.errors, "range")?,
                "default" => {
                    // `meta.value()` returns the stream after `=`. Parse as
                    // an `Expr` so we accept negative literals like
                    // `default = -1` (which `Lit` alone refuses - `-1` is
                    // an `Expr::Unary(Neg, Lit::Int(1))`, not a literal).
                    let expr: Expr = meta.value()?.parse()?;
                    match parse_default_expr(&expr) {
                        Some(value) => {
                            // Preserve the original token stream when the
                            // user wrote a const path (e.g.
                            // `std::f64::consts::SQRT_2`) so clippy
                            // lints against the macro expansion see the
                            // path, not the resolved literal. Numeric
                            // literals keep the existing f64-formatting
                            // pathway.
                            let path_tokens =
                                matches!(expr, Expr::Path(syn::ExprPath { qself: None, .. }))
                                    .then(|| expr.to_token_stream());
                            attrs.default = Some(DefaultExpr { value, path_tokens });
                        }
                        None => push_err(
                            &mut attrs,
                            syn::Error::new_spanned(
                                &expr,
                                "expected a numeric literal or `std::f64::consts::*` constant for \
                                 `default` (e.g. `default = 0.5`, `default = -1`, \
                                 `default = std::f64::consts::SQRT_2`)",
                            ),
                        ),
                    }
                }
                "unit" => take_str_into(&mut attrs.unit, &mut attrs.errors, "unit")?,
                "flags" => take_str_into(&mut attrs.flags, &mut attrs.errors, "flags")?,
                "smooth" => take_str_into(&mut attrs.smooth, &mut attrs.errors, "smooth")?,
                "format" => take_str_into(&mut attrs.format_fn, &mut attrs.errors, "format")?,
                "parse" => take_str_into(&mut attrs.parse_fn, &mut attrs.errors, "parse")?,
                "chunk" => {
                    let value: Lit = meta.value()?.parse()?;
                    match value {
                        Lit::Bool(lit) => attrs.chunk = Some(lit.value),
                        other => push_err(
                            &mut attrs,
                            syn::Error::new_spanned(
                                other,
                                "`#[param(chunk = ...)]` expects a bool literal (e.g. \
                                 `chunk = false` for params too expensive to retarget mid-block)",
                            ),
                        ),
                    }
                }
                "midi_cc" | "midi_source" | "midi_channel" => {
                    parse_midi_attr(key.as_str(), &meta, &mut attrs)?;
                }
                other => {
                    push_err(
                        &mut attrs,
                        meta.error(format!(
                            "unknown `#[param]` key `{other}` (expected one of: id, name, \
                             short_name, group, range, default, unit, flags, smooth, format, \
                             parse, chunk, midi_cc, midi_source, midi_channel)",
                        )),
                    );
                }
            }
            Ok(())
        });
        // `parse_nested_meta` itself can fail at the tokenizer level
        // (mis-typed `=`, stray punctuation). Surface those too.
        if let Err(e) = parse_result {
            push_err(&mut attrs, e);
        }
    }
    attrs
}

/// Check if a field has `#[nested]` attribute.
pub(crate) fn has_nested_attr(field: &syn::Field) -> bool {
    field.attrs.iter().any(|a| a.path().is_ident("nested"))
}

/// Parse the optional `base = N` from `#[nested(base = N)]`. Returns
/// `None` when the attribute is bare (`#[nested]`), which auto-assigns
/// the base.
pub(crate) fn nested_base(field: &syn::Field) -> Option<u32> {
    let attr = field.attrs.iter().find(|a| a.path().is_ident("nested"))?;
    let mut base = None;
    // A bare `#[nested]` has no parens to parse; ignore the error so it
    // falls through to the auto-assigned base.
    let _ = attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("base") {
            let lit: syn::LitInt = meta.value()?.parse()?;
            base = lit.base10_parse::<u32>().ok();
        }
        Ok(())
    });
    base
}

/// Check if a field has `#[meter]` attribute.
pub(crate) fn has_meter_attr(field: &syn::Field) -> bool {
    field.attrs.iter().any(|a| a.path().is_ident("meter"))
}

/// Check if a field type is `MeterSlot`.
pub(crate) fn is_meter_slot(ty: &Type) -> bool {
    type_last_segment(ty).is_some_and(|s| s == "MeterSlot")
}

/// Check if a field has the `#[skip]` attribute. Such fields are not
/// parameters: they carry plugin-owned state the editor reaches through
/// the shared `Arc<Params>` (e.g. a lock-free queue of audio-thread
/// events). They're `Default`-initialized in the generated `new()` and
/// excluded from everything else (ids, infos, state, count).
pub(crate) fn has_skip_attr(field: &syn::Field) -> bool {
    field.attrs.iter().any(|a| a.path().is_ident("skip"))
}

/// A `#[persist]` field: a non-parameter value the host saves alongside
/// the param values (editor-editable config). Like a `#[skip]` field it
/// is `Default`-initialized in `new()` and excluded from ids / infos /
/// count, but its bytes are round-tripped through the generated
/// `serialize_persist` / `load_persist`.
pub(crate) fn has_persist_attr(field: &syn::Field) -> bool {
    field.attrs.iter().any(|a| a.path().is_ident("persist"))
}

/// `#[persist_missing]`: when a non-empty saved blob lacks this field's
/// key, `load_persist` still calls its `persist_read` with an empty
/// cursor so the field can migrate from older documents. Opt-in: empty
/// input has no generic `PersistField` meaning.
pub(crate) fn has_persist_missing_attr(field: &syn::Field) -> bool {
    field
        .attrs
        .iter()
        .any(|a| a.path().is_ident("persist_missing"))
}

/// The `#[persist = "key"]` string key, or the field name when the
/// attribute is bare (`#[persist]`). The key identifies the field in the
/// saved blob so add / remove / reorder stays compatible.
pub(crate) fn persist_key(field: &syn::Field) -> String {
    for attr in &field.attrs {
        if attr.path().is_ident("persist")
            && let syn::Meta::NameValue(nv) = &attr.meta
            && let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
        {
            return s.value();
        }
    }
    field
        .ident
        .as_ref()
        .map_or_else(String::new, std::string::ToString::to_string)
}

/// How auto-assigned parameter IDs are derived when `#[param(id = N)]`
/// is omitted.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdScheme {
    /// Default: a deterministic hash of the field name. Stable under
    /// reordering / inserting fields, so a host's saved automation and
    /// presets keep mapping to the right parameter across versions.
    Hash,
    /// Legacy: a sequential counter from `0` in field-declaration
    /// order. Reordering or inserting a field shifts later IDs and
    /// breaks compatibility with already-saved projects. Opt in with
    /// `#[params(id_scheme = "ordinal")]` for plugins shipped before
    /// the hash default.
    Ordinal,
}

/// Struct-level `#[params(...)]` settings.
pub(crate) struct ParamsStructAttrs {
    pub(crate) persist_changed: Option<syn::Ident>,
    pub(crate) projection: Option<syn::TypePath>,
    pub(crate) id_scheme: IdScheme,
    /// Inherent `fn(&self)` run at the end of `load_persist`, after
    /// values and persist fields are both restored (migrations).
    pub(crate) post_load: Option<syn::Ident>,
    /// Inherent `fn(&self)` run before `collect_values` /
    /// `serialize_persist` (flush editor-side state into fields).
    pub(crate) pre_save: Option<syn::Ident>,
    /// Inherent `fn(&[u8]) -> bool` behind `Params::validate_persist`.
    pub(crate) validate_persist: Option<syn::Ident>,
    /// Inherent `fn(&self, &[(u32, f64)], &[u8])` behind
    /// `Params::restore_state`.
    pub(crate) restore_state: Option<syn::Ident>,
    pub(crate) prepare_persist_stream: Option<syn::Ident>,
    /// Inherent `fn(&self, u32) -> Option<ParameterPresentation>`.
    pub(crate) presentation: Option<syn::Ident>,
    /// Inherent `fn(&self) -> u64`.
    pub(crate) presentation_revision: Option<syn::Ident>,
    pub(crate) presentation_change: Option<syn::Ident>,
    /// Inherent `fn(&self, u32) -> Option<ParamFunction>`.
    pub(crate) function: Option<syn::Ident>,
    /// Inherent `fn(&self, u32, f64) -> Option<String>`; None uses static formatting.
    pub(crate) value_format: Option<syn::Ident>,
    /// Inherent `fn(&self, u32, &str) -> Result<Option<f64>, ()>`.
    pub(crate) value_parse: Option<syn::Ident>,
}

/// Read the struct-level `#[params(...)]` keys. `id_scheme` defaults
/// to [`IdScheme::Hash`]; every hook key names an inherent method.
/// Errors on an unknown key/value.
pub(crate) fn parse_params_struct_attrs(
    attrs: &[syn::Attribute],
) -> Result<ParamsStructAttrs, syn::Error> {
    let mut parsed = ParamsStructAttrs {
        persist_changed: None,
        projection: None,
        id_scheme: IdScheme::Hash,
        post_load: None,
        pre_save: None,
        validate_persist: None,
        restore_state: None,
        prepare_persist_stream: None,
        presentation: None,
        presentation_revision: None,
        presentation_change: None,
        function: None,
        value_format: None,
        value_parse: None,
    };
    for attr in attrs {
        if !attr.path().is_ident("params") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map_or_else(String::new, ToString::to_string);
            let value: syn::LitStr = meta.value()?.parse()?;
            if key == "id_scheme" {
                parsed.id_scheme = match value.value().as_str() {
                    "hash" => IdScheme::Hash,
                    "ordinal" => IdScheme::Ordinal,
                    other => {
                        return Err(meta.error(format!(
                            "unknown `id_scheme` {other:?} (expected \"hash\" or \"ordinal\")"
                        )));
                    }
                };
                return Ok(());
            }
            if key == "projection" {
                if parsed.projection.is_some() { return Err(meta.error("duplicate projection provider")); }
                parsed.projection = Some(syn::parse_str::<syn::TypePath>(&value.value())
                    .map_err(|_| meta.error("projection must name a ParamProjection provider type"))?);
                return Ok(());
            }
            let slot = match key.as_str() {
                "persist_changed" => &mut parsed.persist_changed,
                "post_load" => &mut parsed.post_load,
                "pre_save" => &mut parsed.pre_save,
                "validate_persist" => &mut parsed.validate_persist,
                "restore_state" => &mut parsed.restore_state,
                "prepare_persist_stream" => &mut parsed.prepare_persist_stream,
                "presentation" => &mut parsed.presentation,
                "presentation_revision" => &mut parsed.presentation_revision,
                "presentation_change" => &mut parsed.presentation_change,
                "function" => &mut parsed.function,
                "value_format" => &mut parsed.value_format,
                "value_parse" => &mut parsed.value_parse,
                _ => {
                    return Err(meta.error(
                        "unknown `#[params]` key (expected `id_scheme`, `post_load`, `pre_save`, \
                         `validate_persist`, `restore_state`, `persist_changed`, `presentation`, \
                         `presentation_revision`, `presentation_change`, `function`, `projection`, `value_format`, or `value_parse`)",
                    ));
                }
            };
            *slot = Some(syn::parse_str::<syn::Ident>(&value.value()).map_err(|_| {
                meta.error(format!(
                    "`{key}` must name an inherent method on the Params struct"
                ))
            })?);
            Ok(())
        })?;
    }
    Ok(parsed)
}

/// Deterministic FNV-1a hash of a field name, masked into the
/// historical 24-bit auto-ID space. Pure integer arithmetic
/// over the name bytes, so the value is identical across toolchains,
/// targets, and runs - the property a persisted parameter id needs.
/// `pub(crate)` so the param-index aggregator (`param_index`) flattens
/// nested hash ids with the exact same arithmetic the runtime uses.
pub(crate) fn name_hash_id(name: &str) -> u32 {
    const FNV_OFFSET: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let mut h = FNV_OFFSET;
    for b in name.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h & AUTO_PARAM_ID_MASK
}

/// Full 32-bit FNV-1a of a field name - the keyed `#[derive(State)]`
/// codec's per-field key. Unmasked (unlike [`name_hash_id`], which folds
/// into the param-id space) to minimize collisions; the derive rejects a
/// same-hash pair at compile time.
pub(crate) fn state_field_hash(name: &str) -> u32 {
    const FNV_OFFSET: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let mut h = FNV_OFFSET;
    for b in name.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Coerce a `default = ...` attribute expression into an `f64`.
///
/// Accepts numeric literals (positive and `-`-prefixed) and a
/// whitelisted set of `std::f64::consts::*` paths (also
/// `core::f64::consts::*` and bare `f64::consts::*`). Range and
/// shape checks downstream want a concrete `f64`, so anything
/// outside this shape returns `None` and the caller emits a
/// `compile_error!`.
//
// `i64 as f64` is an at-compile-time literal default whose magnitude
// is bounded by IntParam ranges; large enough to lose mantissa bits
// but well-defined for the validation round-trip.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn parse_default_expr(expr: &Expr) -> Option<f64> {
    match expr {
        Expr::Lit(syn::ExprLit { lit, .. }) => match lit {
            Lit::Float(lit) => lit.base10_parse::<f64>().ok(),
            Lit::Int(lit) => lit.base10_parse::<i64>().ok().map(|n| n as f64),
            // `default = true` / `default = false` for BoolParam map to
            // exactly 1.0 / 0.0; `BoolParam::new` panics on anything
            // else, so this is the only path that produces a valid
            // bool default.
            Lit::Bool(lit) => Some(if lit.value { 1.0 } else { 0.0 }),
            _ => None,
        },
        Expr::Unary(syn::ExprUnary {
            op: UnOp::Neg(_),
            expr: inner,
            ..
        }) => parse_default_expr(inner).map(|v| -v),
        Expr::Path(syn::ExprPath {
            path, qself: None, ..
        }) => std_f64_const(path),
        _ => None,
    }
}

/// Resolve a `[std::|core::]?f64::consts::NAME` path to its `f64`
/// value. Returns `None` for any other shape so the caller can keep
/// using the existing `compile_error!` for unsupported expressions.
///
/// Closed whitelist so the derive's downstream range / shape checks
/// keep working with a concrete `f64` literal embedded in the
/// expansion.
pub(crate) const F64_CONSTS: [(f64, &str); 19] = {
    use std::f64::consts as c;
    [
        (c::PI, "PI"),
        (c::TAU, "TAU"),
        (c::E, "E"),
        (c::SQRT_2, "SQRT_2"),
        (c::FRAC_1_SQRT_2, "FRAC_1_SQRT_2"),
        (c::FRAC_PI_2, "FRAC_PI_2"),
        (c::FRAC_PI_3, "FRAC_PI_3"),
        (c::FRAC_PI_4, "FRAC_PI_4"),
        (c::FRAC_PI_6, "FRAC_PI_6"),
        (c::FRAC_PI_8, "FRAC_PI_8"),
        (c::FRAC_1_PI, "FRAC_1_PI"),
        (c::FRAC_2_PI, "FRAC_2_PI"),
        (c::FRAC_2_SQRT_PI, "FRAC_2_SQRT_PI"),
        (c::LN_2, "LN_2"),
        (c::LN_10, "LN_10"),
        (c::LOG2_E, "LOG2_E"),
        (c::LOG10_E, "LOG10_E"),
        (c::LOG2_10, "LOG2_10"),
        (c::LOG10_2, "LOG10_2"),
    ]
};

pub(crate) fn std_f64_const(path: &syn::Path) -> Option<f64> {
    let segs: Vec<String> = path
        .segments
        .iter()
        .filter(|s| s.arguments.is_none())
        .map(|s| s.ident.to_string())
        .collect();
    if segs.len() != path.segments.len() {
        return None;
    }
    let prefix_ok = match segs.as_slice() {
        [a, b, _] => a == "f64" && b == "consts",
        [a, b, c, _] => (a == "std" || a == "core") && b == "f64" && c == "consts",
        _ => false,
    };
    if !prefix_ok {
        return None;
    }
    let name = segs.last()?;
    F64_CONSTS
        .iter()
        .find(|(_, n)| n == name)
        .map(|(value, _)| *value)
}

/// Collect parameter fields, nested fields, and meter fields from a struct.
pub(crate) type CollectedFields = (
    Vec<ParamField>,
    Vec<NestedField>,
    Vec<MeterField>,
    Vec<syn::Ident>,
);

pub(crate) fn collect_fields(fields: &Fields) -> CollectedFields {
    let Fields::Named(named) = fields else {
        return (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    };

    let mut params = Vec::new();
    let mut nested = Vec::new();
    let mut meters = Vec::new();
    let mut skipped = Vec::new();

    for f in &named.named {
        let Some(ident) = f.ident.clone() else {
            continue;
        };

        // Checked first: an explicit `#[skip]` opt-out (or a
        // `#[persist]` saved-config field) always wins over whatever the
        // field's type would otherwise classify as. Both are non-params,
        // `Default`-initialized in `new()`; `#[persist]` fields are also
        // collected separately for the serialize/load-persist codegen.
        if has_skip_attr(f) || has_persist_attr(f) {
            skipped.push(ident);
            continue;
        }

        if has_nested_attr(f) {
            nested.push(NestedField {
                ident,
                ty: f.ty.clone(),
                base: nested_base(f),
            });
            continue;
        }

        if has_meter_attr(f) || is_meter_slot(&f.ty) {
            meters.push(MeterField { ident, id: None });
            continue;
        }

        if let Type::Array(array) = &f.ty
            && let Some(kind) = classify_param_type(&array.elem)
        {
            let enum_type = (kind == ParamKind::Enum)
                .then(|| extract_enum_type_arg(&array.elem))
                .flatten();
            params.extend(array_elements(f, ident, kind, enum_type, array));
            continue;
        }

        if let Some(kind) = classify_param_type(&f.ty) {
            let attrs = parse_param_attrs(f);
            let enum_type = if kind == ParamKind::Enum {
                extract_enum_type_arg(&f.ty)
            } else {
                None
            };
            params.push(ParamField {
                ident,
                kind,
                attrs,
                enum_type,
                index: None,
            });
        }
    }

    (params, nested, meters, skipped)
}

/// An array field's elements, one [`ParamField`] each. They share the
/// field's attributes but for three: an explicit `id = N` is element 0's,
/// element `i` taking `N + i`; `name` and `short_name` are templates in
/// which `{i}` is the element's index from 0 and `{n}` its number from 1,
/// either optionally zero-padded (`{n:03}`); and a MIDI binding is refused
/// (every element would claim the same source). The length must be an
/// integer literal: the derive emits each element's metadata.
pub(crate) fn array_elements(
    f: &syn::Field,
    ident: syn::Ident,
    kind: ParamKind,
    enum_type: Option<syn::Type>,
    array: &syn::TypeArray,
) -> Vec<ParamField> {
    let mut attrs = parse_param_attrs(f);
    let len = match &array.len {
        Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(n),
            ..
        }) => n.base10_parse::<usize>().ok(),
        _ => None,
    };
    let error = |attrs: &mut ParamAttrs, msg: &str| {
        attrs
            .errors
            .push(syn::Error::new_spanned(&f.ty, msg).to_compile_error());
    };
    let Some(len) = len.filter(|&n| n > 0) else {
        error(
            &mut attrs,
            "a param array's length must be a positive integer literal",
        );
        return vec![ParamField {
            ident,
            kind,
            attrs,
            enum_type,
            index: Some(0),
        }];
    };
    if attrs.midi_map.is_some() {
        error(
            &mut attrs,
            "a param array cannot take `midi_cc` / `midi_source`: every element would claim the same source",
        );
    }
    let template = attrs.name.clone().unwrap_or_default();
    if !has_index_placeholder(&template) {
        error(
            &mut attrs,
            "a param array's `name` needs `{i}` (index from 0) or `{n}` (number from 1), so each element is named apart",
        );
    }
    (0..len)
        .map(|i| {
            let mut a = attrs.clone();
            if i > 0 {
                // Reported once, on the first element.
                a.errors.clear();
            }
            a.id = attrs
                .id
                .map(|base| base.saturating_add(u32::try_from(i).unwrap_or(u32::MAX)));
            a.name = attrs.name.as_deref().map(|t| fill_index(t, i));
            a.short_name = attrs.short_name.as_deref().map(|t| fill_index(t, i));
            ParamField {
                ident: ident.clone(),
                kind,
                attrs: a,
                enum_type: enum_type.clone(),
                index: Some(i),
            }
        })
        .collect()
}

/// Whether `template` names an element: holds `{i}` or `{n}`, padded or not.
pub(crate) fn has_index_placeholder(template: &str) -> bool {
    fill_index(template, 0) != fill_index(template, 1)
}

/// `template` with each `{i}` / `{n}` (or `{i:0W}` / `{n:0W}`) replaced by
/// element `index`'s index from 0 / number from 1, zero-padded to `W`.
pub(crate) fn fill_index(template: &str, index: usize) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let spec = &after[..close];
        let (key, width) = spec.split_once(':').map_or((spec, ""), |(k, w)| (k, w));
        let value = match key {
            "i" => Some(index),
            "n" => Some(index + 1),
            _ => None,
        };
        let width = width
            .strip_prefix('0')
            .map_or(Some(0), |w| w.parse::<usize>().ok());
        if let (Some(v), Some(w)) = (value, width) {
            let _ = write!(out, "{v:0w$}");
            rest = &after[close + 1..];
        } else {
            // Not a placeholder: keep the brace and read on from the next
            // character, so `open { {n}` still fills its `{n}`.
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod array_name_tests {
    use super::*;

    #[test]
    fn templates_fill_index_number_and_padding() {
        assert_eq!(fill_index("#{i:03}", 7), "#007");
        assert_eq!(fill_index("Macro {n}", 0), "Macro 1");
        assert_eq!(fill_index("{n:02} of {i}", 9), "10 of 9");
        // Other braces stay as written.
        assert_eq!(fill_index("{x} {n}", 1), "{x} 2");
        assert_eq!(fill_index("open { {n}", 0), "open { 1");
        assert_eq!(fill_index("tail {", 0), "tail {");
    }

    #[test]
    fn a_name_without_an_index_is_refused() {
        assert!(has_index_placeholder("Slot {n}"));
        assert!(has_index_placeholder("#{i:03}"));
        assert!(!has_index_placeholder("Slot"));
        assert!(!has_index_placeholder("{x}"));
    }
}
