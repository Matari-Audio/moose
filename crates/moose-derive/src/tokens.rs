//! Token builders for parameter ranges, units, flags and literals.

#[cfg(test)]
use crate::fields::DefaultExpr;
use crate::fields::F64_CONSTS;
use crate::fields::ParamField;
use crate::fields::ParamKind;

use quote::quote;

/// Parse a range string like "linear(-60, 24)" into tokens.
/// Emit an `f64` as a decimal literal (`60.0`, `-60.0`) rather than the
/// suffixed-integer form `quote! { #f64 }` produces for whole numbers
/// (`60f64`). rustc accepts `60f64`, but rust-analyzer's proc-macro
/// expansion (reported on 1.96) rejects it with "expected Expr". The
/// decimal form round-trips cleanly and matches the hand-written
/// `0.0` / `1.0` literals the range-less default path emits.
pub(crate) fn f64_lit(v: f64) -> proc_macro2::TokenStream {
    // A `default = std::f64::consts::X` comes back out as the path: its
    // shortest decimal is exact, but it trips clippy's `approx_constant`
    // in every crate that expands the derive.
    for (value, name) in F64_CONSTS {
        if v.abs().to_bits() == value.to_bits() {
            let ident = syn::Ident::new(name, proc_macro2::Span::call_site());
            return if v < 0.0 {
                quote! { -::core::f64::consts::#ident }
            } else {
                quote! { ::core::f64::consts::#ident }
            };
        }
    }
    if v < 0.0 {
        let abs = proc_macro2::Literal::f64_unsuffixed(-v);
        quote! { -#abs }
    } else {
        let lit = proc_macro2::Literal::f64_unsuffixed(v);
        quote! { #lit }
    }
}

/// Like [`f64_lit`] for `i64` - emits `12` / `-12`, not `12i64`.
pub(crate) fn i64_lit(v: i64) -> proc_macro2::TokenStream {
    let abs = proc_macro2::Literal::u64_unsuffixed(v.unsigned_abs());
    if v < 0 {
        quote! { -#abs }
    } else {
        quote! { #abs }
    }
}

/// Like [`f64_lit`] for `usize` - emits `2`, not `2usize`.
pub(crate) fn usize_lit(v: usize) -> proc_macro2::TokenStream {
    let lit = proc_macro2::Literal::usize_unsuffixed(v);
    quote! { #lit }
}

/// Parse `offset_log(min, max, offset)` and `pivoted_log(min, max, mid)`.
/// `None` when neither prefix matches; `Some(compile_error!)` on a
/// malformed match.
pub(crate) fn parse_log_range(range: &str) -> Option<proc_macro2::TokenStream> {
    let bad = |msg: String| quote! { compile_error!(#msg) };
    let args = |prefix: &str| -> Option<Vec<String>> {
        range
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(')'))
            .map(|inner| inner.split(',').map(|p| p.trim().to_owned()).collect())
    };

    if let Some(parts) = args("offset_log(") {
        let nums: Vec<f64> = parts.iter().filter_map(|p| p.parse().ok()).collect();
        if parts.len() != 3 || nums.len() != 3 {
            return Some(bad(format!(
                "offset_log range needs three numbers `offset_log(min, max, offset)`, got `{range}`"
            )));
        }
        let (min, max, offset) = (nums[0], nums[1], nums[2]);
        if !min.is_finite()
            || !max.is_finite()
            || !offset.is_finite()
            || min >= max
            || min + offset <= 0.0
            || !(min + offset).is_finite()
            || !(max + offset).is_finite()
        {
            return Some(bad(format!(
                "offset_log range needs finite min < max and finite shifted bounds with min + offset > 0, got `{range}`"
            )));
        }
        let (min, max, offset) = (f64_lit(min), f64_lit(max), f64_lit(offset));
        return Some(quote! {
            ::moose::params::ParamRange::OffsetLog { min: #min, max: #max, offset: #offset }
        });
    }
    if let Some(parts) = args("pivoted_log(") {
        let nums: Vec<f64> = parts.iter().filter_map(|p| p.parse().ok()).collect();
        if parts.len() != 3 || nums.len() != 3 {
            return Some(bad(format!(
                "pivoted_log range needs three numbers `pivoted_log(min, max, mid)`, got `{range}`"
            )));
        }
        let (min, max, mid) = (nums[0], nums[1], nums[2]);
        if !min.is_finite()
            || !max.is_finite()
            || !mid.is_finite()
            || min <= 0.0
            || mid <= min
            || max <= mid
        {
            return Some(bad(format!(
                "pivoted_log range needs finite 0 < min < mid < max, got `{range}`"
            )));
        }
        let (min, max, mid) = (f64_lit(min), f64_lit(max), f64_lit(mid));
        return Some(quote! {
            ::moose::params::ParamRange::PivotedLog { min: #min, max: #max, mid: #mid }
        });
    }
    None
}

/// Parse the two power-law taper shapes (`skewed(min, max, factor)` and
/// `sym_skewed(min, max, factor, center)`). `None` when neither prefix
/// matches; `Some(compile_error!)` on a malformed match. Split out of
/// [`parse_range_tokens`] to keep that function under the line-length lint.
pub(crate) fn parse_skew_range(range: &str) -> Option<proc_macro2::TokenStream> {
    let bad = |msg: String| quote! { compile_error!(#msg) };
    // Reject non-positive and non-finite factors without a `!(x > 0.0)`
    // (which clippy flags on partially-ordered floats).
    let bad_factor = |factor: f64| factor <= 0.0 || !factor.is_finite();

    if let Some(inner) = range
        .strip_prefix("skewed(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() != 3 {
            return Some(bad(format!(
                "skewed range needs three arguments `skewed(min, max, factor)`, got `skewed({inner})`"
            )));
        }
        let Ok(min) = parts[0].parse::<f64>() else {
            return Some(bad(format!(
                "skewed range min `{}` is not a number",
                parts[0]
            )));
        };
        let Ok(max) = parts[1].parse::<f64>() else {
            return Some(bad(format!(
                "skewed range max `{}` is not a number",
                parts[1]
            )));
        };
        let Ok(factor) = parts[2].parse::<f64>() else {
            return Some(bad(format!(
                "skewed range factor `{}` is not a number",
                parts[2]
            )));
        };
        if !min.is_finite() || !max.is_finite() || min >= max {
            return Some(bad(format!(
                "skewed range needs finite min < max, got `skewed({min}, {max}, {factor})`"
            )));
        }
        if bad_factor(factor) {
            return Some(bad(format!(
                "skewed range needs a finite, strictly positive factor, got `{factor}`"
            )));
        }
        let (min, max, factor) = (f64_lit(min), f64_lit(max), f64_lit(factor));
        return Some(quote! {
            ::moose::params::ParamRange::Skewed { min: #min, max: #max, factor: #factor }
        });
    }
    if let Some(inner) = range
        .strip_prefix("sym_skewed(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() != 4 {
            return Some(bad(format!(
                "sym_skewed range needs four arguments `sym_skewed(min, max, factor, center)`, \
                 got `sym_skewed({inner})`"
            )));
        }
        let Ok(min) = parts[0].parse::<f64>() else {
            return Some(bad(format!(
                "sym_skewed range min `{}` is not a number",
                parts[0]
            )));
        };
        let Ok(max) = parts[1].parse::<f64>() else {
            return Some(bad(format!(
                "sym_skewed range max `{}` is not a number",
                parts[1]
            )));
        };
        let Ok(factor) = parts[2].parse::<f64>() else {
            return Some(bad(format!(
                "sym_skewed range factor `{}` is not a number",
                parts[2]
            )));
        };
        let Ok(center) = parts[3].parse::<f64>() else {
            return Some(bad(format!(
                "sym_skewed range center `{}` is not a number",
                parts[3]
            )));
        };
        if !min.is_finite() || !max.is_finite() || min >= max {
            return Some(bad(format!(
                "sym_skewed range needs finite min < max, got `sym_skewed({min}, {max}, {factor}, {center})`"
            )));
        }
        if bad_factor(factor) {
            return Some(bad(format!(
                "sym_skewed range needs a finite, strictly positive factor, got `{factor}`"
            )));
        }
        if !center.is_finite() || center <= min || center >= max {
            return Some(bad(format!(
                "sym_skewed center must be finite and strictly between min and max, got center={center} \
                 for [{min}, {max}]"
            )));
        }
        let (min, max, factor, center) =
            (f64_lit(min), f64_lit(max), f64_lit(factor), f64_lit(center));
        return Some(quote! {
            ::moose::params::ParamRange::SymmetricalSkewed {
                min: #min, max: #max, factor: #factor, center: #center
            }
        });
    }
    None
}

pub(crate) fn parse_range_tokens(range: &str) -> proc_macro2::TokenStream {
    let bad = |msg: String| quote! { compile_error!(#msg) };

    if let Some(inner) = range
        .strip_prefix("linear(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() != 2 {
            return bad(format!(
                "linear range needs two arguments `linear(min, max)`, got `linear({inner})`"
            ));
        }
        let Ok(min) = parts[0].parse::<f64>() else {
            return bad(format!("linear range min `{}` is not a number", parts[0]));
        };
        let Ok(max) = parts[1].parse::<f64>() else {
            return bad(format!("linear range max `{}` is not a number", parts[1]));
        };
        if !min.is_finite() || !max.is_finite() || min >= max {
            return bad(format!(
                "linear range needs finite min < max, got `linear({min}, {max})`"
            ));
        }
        let (min, max) = (f64_lit(min), f64_lit(max));
        return quote! { ::moose::params::ParamRange::Linear { min: #min, max: #max } };
    }
    if let Some(inner) = range
        .strip_prefix("stepped(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() != 3 {
            return bad(format!(
                "stepped range needs three arguments `stepped(min, max, step)`, got `stepped({inner})`"
            ));
        }
        let Ok(min) = parts[0].parse::<f64>() else {
            return bad(format!("stepped range min `{}` is not a number", parts[0]));
        };
        let Ok(max) = parts[1].parse::<f64>() else {
            return bad(format!("stepped range max `{}` is not a number", parts[1]));
        };
        let Ok(step) = parts[2].parse::<f64>() else {
            return bad(format!("stepped range step `{}` is not a number", parts[2]));
        };
        if !min.is_finite() || !max.is_finite() || !step.is_finite() || min >= max || step <= 0.0 {
            return bad(format!(
                "stepped range needs finite min < max and step > 0, got `stepped({min}, {max}, {step})`"
            ));
        }
        let (min, max, step) = (f64_lit(min), f64_lit(max), f64_lit(step));
        return quote! { ::moose::params::ParamRange::Stepped { min: #min, max: #max, step: #step } };
    }
    if let Some(inner) = range.strip_prefix("log(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() != 2 {
            return bad(format!(
                "log range needs two arguments `log(min, max)`, got `log({inner})`"
            ));
        }
        let Ok(min) = parts[0].parse::<f64>() else {
            return bad(format!("log range min `{}` is not a number", parts[0]));
        };
        let Ok(max) = parts[1].parse::<f64>() else {
            return bad(format!("log range max `{}` is not a number", parts[1]));
        };
        if !min.is_finite() || !max.is_finite() || min <= 0.0 || max <= 0.0 {
            return bad(format!(
                "log range needs finite, strictly positive bounds, got `log({min}, {max})`"
            ));
        }
        if min >= max {
            return bad(format!(
                "log range needs min < max, got `log({min}, {max})`"
            ));
        }
        let (min, max) = (f64_lit(min), f64_lit(max));
        return quote! { ::moose::params::ParamRange::Logarithmic { min: #min, max: #max } };
    }
    if let Some(inner) = range
        .strip_prefix("discrete(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() != 2 {
            return bad(format!(
                "discrete range needs two arguments `discrete(min, max)`, got `discrete({inner})`"
            ));
        }
        let Ok(min) = parts[0].parse::<i64>() else {
            return bad(format!(
                "discrete range min `{}` is not an integer",
                parts[0]
            ));
        };
        let Ok(max) = parts[1].parse::<i64>() else {
            return bad(format!(
                "discrete range max `{}` is not an integer",
                parts[1]
            ));
        };
        if min >= max {
            return bad(format!(
                "discrete range needs min < max, got `discrete({min}, {max})`"
            ));
        }
        let (min, max) = (i64_lit(min), i64_lit(max));
        return quote! { ::moose::params::ParamRange::Discrete { min: #min, max: #max } };
    }
    if let Some(inner) = range
        .strip_prefix("enum(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let Ok(count) = inner.trim().parse::<usize>() else {
            return bad(format!(
                "enum count `{}` is not a non-negative integer",
                inner.trim()
            ));
        };
        if count < 2 {
            return bad(format!(
                "enum range needs at least 2 variants, got `enum({count})`"
            ));
        }
        let count = usize_lit(count);
        return quote! { ::moose::params::ParamRange::Enum { count: #count } };
    }
    if let Some(tokens) = parse_log_range(range) {
        return tokens;
    }
    if let Some(tokens) = parse_skew_range(range) {
        return tokens;
    }
    if let Some(inner) = range
        .strip_prefix("reversed(")
        .and_then(|s| s.strip_suffix(')'))
    {
        // Recursively parse the wrapped shape and flip it. The inner is a
        // const expression, so `&` promotes it to the `&'static` the
        // `Reversed` variant holds.
        let inner_tokens = parse_range_tokens(inner.trim());
        return quote! { ::moose::params::ParamRange::Reversed(&#inner_tokens) };
    }
    bad(format!(
        "unknown range `{range}` - supported: linear(min, max), log(min, max), \
         stepped(min, max, step), offset_log(min, max, offset), pivoted_log(min, max, mid), skewed(min, max, factor), sym_skewed(min, max, factor, center), \
         discrete(min, max), enum(count), reversed(<range>)"
    ))
}

/// The concrete `(min, max)` a range string spans, for expansion-time
/// default validation. `None` when the bounds aren't statically known -
/// a rangeless param, or an `enum(N)` whose count comes from a
/// cross-crate sidecar rather than a literal.
pub(crate) fn range_bounds(range: &str) -> Option<(f64, f64)> {
    let range = range.trim();
    if let Some(inner) = range
        .strip_prefix("reversed(")
        .and_then(|s| s.strip_suffix(')'))
    {
        // Reversal flips only the mapping; the numeric bounds are the same.
        return range_bounds(inner);
    }
    let leading_pair = |inner: &str| -> Option<(f64, f64)> {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        let lo = parts.first()?.parse::<f64>().ok()?;
        let hi = parts.get(1)?.parse::<f64>().ok()?;
        (lo.is_finite() && hi.is_finite()).then(|| (lo.min(hi), lo.max(hi)))
    };
    for prefix in [
        "linear(",
        "stepped(",
        "log(",
        "offset_log(",
        "pivoted_log(",
        "skewed(",
        "sym_skewed(",
    ] {
        if let Some(inner) = range.strip_prefix(prefix).and_then(|s| s.strip_suffix(')')) {
            return leading_pair(inner);
        }
    }
    if let Some(inner) = range
        .strip_prefix("discrete(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        let lo = parts.first()?.parse::<i64>().ok()?;
        let hi = parts.get(1)?.parse::<i64>().ok()?;
        #[allow(clippy::cast_precision_loss)]
        return Some((lo.min(hi) as f64, lo.max(hi) as f64));
    }
    if let Some(inner) = range
        .strip_prefix("enum(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let count = inner.trim().parse::<u32>().ok()?;
        return (count >= 1).then(|| (0.0, f64::from(count - 1)));
    }
    None
}

/// Compile-time range-containment check for a field's default. `Some`
/// message when the default (explicit, or the omitted `0.0` fallback)
/// sits outside the declared range - which panics at instantiation for
/// Int / Enum / Bool and ships mis-displayed DSP for Float. `None` for
/// enum *path* defaults (in range by construction) and shapes whose
/// bounds aren't statically known.
pub(crate) fn default_range_error(f: &ParamField) -> Option<String> {
    let a = &f.attrs;
    let (lo, hi) = range_bounds(a.range.as_deref()?)?;
    let effective = a.default.as_ref().map_or(0.0, |d| d.value);
    if !effective.is_finite() || (effective >= lo && effective <= hi) {
        return None;
    }
    let name = a.name.as_deref().unwrap_or("Unnamed");
    Some(if a.default.is_some() {
        format!("`{name}` default {effective} is outside its range [{lo}, {hi}]")
    } else {
        format!(
            "`{name}` has no `default` and its range [{lo}, {hi}] doesn't contain 0.0 - \
             add `default = <value in range>`"
        )
    })
}

/// Parse a unit string into `ParamUnit` tokens.
pub(crate) fn parse_unit_tokens(unit: &str) -> proc_macro2::TokenStream {
    if let Some(suffix) = unit.strip_prefix("custom:") {
        return quote! { ::moose::params::ParamUnit::Custom(#suffix) };
    }
    match unit {
        "dB" | "Db" | "db" => quote! { ::moose::params::ParamUnit::Db },
        "Hz" | "hz" => quote! { ::moose::params::ParamUnit::Hz },
        "ms" => quote! { ::moose::params::ParamUnit::Milliseconds },
        "s" => quote! { ::moose::params::ParamUnit::Seconds },
        "%" => quote! { ::moose::params::ParamUnit::Percent },
        "st" => quote! { ::moose::params::ParamUnit::Semitones },
        "pan" => quote! { ::moose::params::ParamUnit::Pan },
        "deg" | "°" => quote! { ::moose::params::ParamUnit::Degrees },
        "" | "none" => quote! { ::moose::params::ParamUnit::None },
        // Loud compile-error rather than silent fallback - typos like
        // `"hz "` (trailing space) or `"DB"` (uppercase) shouldn't map
        // to `ParamUnit::None` and surface only as "0.5" instead of
        // "0.5 Hz" in the host.
        other => {
            let msg = format!(
                "unknown unit `{other}` - supported: dB, Hz, ms, s, %, st, pan, deg, none, custom:<suffix>"
            );
            quote! { compile_error!(#msg) }
        }
    }
}

/// Parse a flags string into `ParamFlags` tokens.
pub(crate) fn parse_flags_tokens(flags: &str) -> proc_macro2::TokenStream {
    let mut parts = Vec::new();
    for flag in flags.split('|').map(|s| s.trim().to_lowercase()) {
        match flag.as_str() {
            "automatable" => parts.push(quote! { ::moose::params::ParamFlags::AUTOMATABLE }),
            "none" | "non_automatable" => {
                parts.push(quote! { ::moose::params::ParamFlags::empty() });
            }
            "hidden" => parts.push(quote! { ::moose::params::ParamFlags::HIDDEN }),
            "readonly" => parts.push(quote! { ::moose::params::ParamFlags::READONLY }),
            "bypass" => parts.push(quote! { ::moose::params::ParamFlags::IS_BYPASS }),
            "modulatable" => parts.push(quote! { ::moose::params::ParamFlags::MODULATABLE }),
            // Per-note modulation implies mono modulation.
            "modulatable_per_note" => parts.push(quote! {
                ::moose::params::ParamFlags::MODULATABLE
                    | ::moose::params::ParamFlags::MODULATABLE_PER_NOTE
            }),
            // Tolerate empty segments (a trailing `|`); reject typos loudly
            // like every sibling parser, so a `read_only` -> `readonly`
            // slip can't silently ship an automatable "read-only" param.
            "" => {}
            other => {
                let msg = format!(
                    "unknown param flag `{other}` - supported: none, automatable, non_automatable, hidden, \
                     readonly, bypass, modulatable, modulatable_per_note",
                );
                return quote! { compile_error!(#msg) };
            }
        }
    }
    if parts.is_empty() {
        quote! { ::moose::params::ParamFlags::AUTOMATABLE }
    } else {
        quote! { #(#parts)|* }
    }
}

/// Parse a smoothing string into `SmoothingStyle` tokens. Same
/// loud-on-malformed contract as `parse_unit_tokens` /
/// `parse_range_tokens`: every typo emits a `compile_error!` instead
/// of silently swallowing the bad value.
pub(crate) fn parse_smooth_tokens(smooth: &str) -> proc_macro2::TokenStream {
    let bad = |msg: String| quote! { compile_error!(#msg) };
    if smooth == "none" {
        return quote! { ::moose::params::SmoothingStyle::None };
    }
    if let Some(inner) = smooth
        .strip_prefix("linear(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return match inner.trim().parse::<f64>() {
            Ok(ms) => quote! { ::moose::params::SmoothingStyle::Linear(#ms) },
            Err(_) => bad(format!(
                "smooth = \"linear({inner})\" expects a numeric milliseconds value \
                 (e.g. `smooth = \"linear(20)\"`)",
            )),
        };
    }
    if let Some(inner) = smooth
        .strip_prefix("exp(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return match inner.trim().parse::<f64>() {
            Ok(ms) => quote! { ::moose::params::SmoothingStyle::Exponential(#ms) },
            Err(_) => bad(format!(
                "smooth = \"exp({inner})\" expects a numeric milliseconds value \
                 (e.g. `smooth = \"exp(5)\"`)",
            )),
        };
    }
    if let Some(inner) = smooth
        .strip_prefix("log(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return match inner.trim().parse::<f64>() {
            Ok(ms) => quote! { ::moose::params::SmoothingStyle::Logarithmic(#ms) },
            Err(_) => bad(format!(
                "smooth = \"log({inner})\" expects a numeric milliseconds value \
                 (e.g. `smooth = \"log(20)\"` for multiplicative smoothing)",
            )),
        };
    }
    bad(format!(
        "unknown smoothing style `{smooth}` - supported: \"none\", \"linear(<ms>)\", \
         \"exp(<ms>)\", \"log(<ms>)\"",
    ))
}

/// Build the `ParamInfo { ... }` literal for a `#[param(...)]` field.
///
/// Shared between [`gen_field_constructor`] (which wraps it in a
/// `FloatParam`/`BoolParam`/etc. constructor at runtime) and the
/// derive's static-metadata path
/// ([`Params::param_infos_static`](moose_params::Params::param_infos_static)),
/// which lifts the same literal into a `LazyLock<Vec<ParamInfo>>` so
/// format wrappers can read parameter metadata without constructing a
/// plugin instance. Returns `None` when a compile-time validation
/// (default-out-of-range etc.) failed; the caller's `compile_error!`
/// path handles that branch.
pub(crate) fn gen_param_info_literal(f: &ParamField) -> Option<proc_macro2::TokenStream> {
    let a = &f.attrs;
    let id = f.id();
    let name = a.name.as_deref().unwrap_or("Unnamed");
    let short_name = a.short_name.as_deref().unwrap_or(name);
    let group = a.group.as_deref().unwrap_or("");

    if let Some(d) = a.default.as_ref().map(|d| d.value) {
        // Integer round-trip exactness checks - an epsilon-based
        // comparison would silently accept fractional defaults like
        // `2.5` for an `Int` / `Enum` param. The `as i64` / `as u32`
        // truncations are the round-trip's whole point.
        #[allow(
            clippy::float_cmp,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let invalid = match f.kind {
            ParamKind::Bool => d != 0.0 && d != 1.0,
            ParamKind::Int => !d.is_finite() || (d as i64 as f64) != d,
            ParamKind::Enum => !d.is_finite() || d < 0.0 || f64::from(d as u32) != d,
            ParamKind::Float => !d.is_finite(),
        };
        if invalid {
            return None;
        }
    }

    let default_plain = a
        .default
        .as_ref()
        .map_or_else(|| quote! { 0.0 }, |d| f64_lit(d.value));

    let range = match &a.range {
        Some(r) => parse_range_tokens(r),
        None => match f.kind {
            ParamKind::Bool => quote! { ::moose::params::ParamRange::Discrete { min: 0, max: 1 } },
            ParamKind::Enum => {
                if let Some(ref enum_ty) = f.enum_type {
                    quote! { ::moose::params::ParamRange::Enum { count: <#enum_ty as ::moose::params::ParamEnum>::variant_count() } }
                } else {
                    quote! { ::moose::params::ParamRange::Enum { count: 2 } }
                }
            }
            _ => quote! { ::moose::params::ParamRange::Linear { min: 0.0, max: 1.0 } },
        },
    };

    let unit = if let Some(u) = &a.unit {
        parse_unit_tokens(u)
    } else {
        quote! { ::moose::params::ParamUnit::None }
    };

    // The explicit-flags path lets a plugin pass `flags = "hidden |
    // bypass"` to override AUTOMATABLE; OR in CHUNKED on the default
    // path (and on the explicit path unless the plugin opted out via
    // `chunk = false`) so the wrapper-side chunker treats every param
    // as a split point by default. See the
    // `parameter-dependent-chunking.md` design doc.
    let base_flags = if let Some(fl) = &a.flags {
        parse_flags_tokens(fl)
    } else {
        quote! { ::moose::params::ParamFlags::AUTOMATABLE }
    };
    let flags = if a.chunk.unwrap_or(true) {
        quote! { (#base_flags).union(::moose::params::ParamFlags::CHUNKED) }
    } else {
        base_flags
    };

    let kind = match f.kind {
        ParamKind::Float => quote! { ::moose::params::ParamValueKind::Float },
        ParamKind::Int => quote! { ::moose::params::ParamValueKind::Int },
        ParamKind::Bool => quote! { ::moose::params::ParamValueKind::Bool },
        ParamKind::Enum => quote! { ::moose::params::ParamValueKind::Enum },
    };

    let midi_map = a.midi_map.map_or_else(
        || quote! { None },
        |k| {
            let src = k.to_tokens();
            quote! { Some(#src) }
        },
    );
    let midi_channel = a.midi_channel.map_or_else(
        || quote! { None },
        |ch| {
            let lit = proc_macro2::Literal::u8_unsuffixed(ch);
            quote! { Some(#lit) }
        },
    );

    Some(quote! {
        ::moose::params::ParamInfo {
            id: #id,
            name: #name,
            short_name: #short_name,
            group: #group,
            range: #range,
            default_plain: #default_plain,
            flags: #flags,
            unit: #unit,
            kind: #kind,
            midi_map: #midi_map,
            midi_channel: #midi_channel,
        }
    })
}

/// Generate a constructor call for a field with `#[param(...)]` attributes.
///
/// `f.id()` carries the `expect`-guarded "must run after auto-assign"
/// invariant; using it here surfaces the order-of-call contract at
/// construction time instead of silently minting `id = 0`. Today every
/// caller drives this via `assign_param_ids` first, but a future
/// refactor that calls `gen_field_constructor` out of order panics
/// with a precise message rather than producing colliding id=0 params.
pub(crate) fn gen_field_constructor(f: &ParamField, slot: usize) -> proc_macro2::TokenStream {
    let a = &f.attrs;
    let name = a.name.as_deref().unwrap_or("Unnamed");

    // Compile-time sanity check on `default = ...`. Surfaces user
    // errors that would otherwise silently saturate at runtime (`as
    // u32` on a negative `default_plain`, `as i64` on a fractional
    // value). The variant-count range check for `EnumParam` still
    // runs at construction time because `variant_count()` isn't
    // visible to the macro at expansion time without per-call
    // const-eval plumbing.
    if let Some(d) = a.default.as_ref().map(|d| d.value) {
        // Integer round-trip exactness checks - an epsilon-based
        // comparison would silently accept fractional defaults like
        // `2.5` for an `Int` / `Enum` param. The `as i64` / `as u32`
        // truncations are the round-trip's whole point.
        #[allow(
            clippy::float_cmp,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let err = match f.kind {
            ParamKind::Bool if d != 0.0 && d != 1.0 => {
                Some(format!("BoolParam default {name} must be 0 or 1; got {d}"))
            }
            ParamKind::Int if !d.is_finite() || (d as i64 as f64) != d => Some(format!(
                "IntParam '{name}' default must be an integer literal; got {d}"
            )),
            ParamKind::Enum if !d.is_finite() || d < 0.0 || f64::from(d as u32) != d => {
                Some(format!(
                    "EnumParam '{name}' default must be a non-negative integer (variant index); got {d}"
                ))
            }
            ParamKind::Float if !d.is_finite() => Some(format!(
                "FloatParam '{name}' default must be finite; got {d}"
            )),
            _ => None,
        };
        if let Some(msg) = err {
            return quote! { compile_error!(#msg) };
        }
    }

    // Range containment - lands in `field: <expr>` position, so no `;`.
    if let Some(msg) = default_range_error(f) {
        return quote! { compile_error!(#msg) };
    }

    if gen_param_info_literal(f).is_none() {
        // Validation block above already returned a `compile_error!`
        // for every shape that `gen_param_info_literal` rejects.
        // Surface a fallback diagnostic so a future divergence
        // between the two checks fails loudly instead of silently
        // emitting bad code.
        let msg = format!("invalid `#[param]` attributes on field `{name}`");
        return quote! { compile_error!(#msg) };
    }
    // The metadata lives in the struct's static table (see `new()`);
    // the param borrows its slot and carries only its id.
    let info = quote! { &__MOOSE_PARAM_INFOS[#slot] };

    match f.kind {
        ParamKind::Float => {
            let smooth = if let Some(s) = &a.smooth {
                parse_smooth_tokens(s)
            } else {
                quote! { ::moose::params::SmoothingStyle::None }
            };
            quote! { ::moose::params::FloatParam::new(#info, #smooth) }
        }
        ParamKind::Bool => quote! { ::moose::params::BoolParam::new(#info) },
        ParamKind::Int => quote! { ::moose::params::IntParam::new(#info) },
        ParamKind::Enum => quote! { ::moose::params::EnumParam::new(#info) },
    }
}

#[cfg(test)]
mod parse_default_tests {
    use super::*;
    use crate::fields::parse_default_expr;

    fn eval(src: &str) -> Option<f64> {
        let expr: syn::Expr = syn::parse_str(src).expect("test input must parse as Expr");
        parse_default_expr(&expr)
    }

    #[test]
    fn numeric_literals() {
        assert_eq!(eval("0.5"), Some(0.5));
        assert_eq!(eval("3"), Some(3.0));
        assert_eq!(eval("-1"), Some(-1.0));
        assert_eq!(eval("-0.25"), Some(-0.25));
        assert_eq!(eval("true"), Some(1.0));
        assert_eq!(eval("false"), Some(0.0));
    }

    #[test]
    fn f64_lit_emits_std_consts_as_paths() {
        let lit = |v: f64| f64_lit(v).to_string().replace(' ', "");
        assert_eq!(
            lit(std::f64::consts::FRAC_1_SQRT_2),
            "::core::f64::consts::FRAC_1_SQRT_2"
        );
        assert_eq!(lit(-std::f64::consts::PI), "-::core::f64::consts::PI");
        assert_eq!(lit(0.5), "0.5");
    }

    #[test]
    fn flags_none_is_empty() {
        let tokens = parse_flags_tokens("none").to_string().replace(' ', "");
        assert_eq!(tokens, "::moose::params::ParamFlags::empty()");
    }

    #[test]
    fn std_f64_const_paths() {
        // All three accepted prefixes resolve to the same value.
        let pi = std::f64::consts::PI;
        assert_eq!(eval("std::f64::consts::PI"), Some(pi));
        assert_eq!(eval("core::f64::consts::PI"), Some(pi));
        assert_eq!(eval("f64::consts::PI"), Some(pi));
        // Negation composes with `Expr::Unary`.
        assert_eq!(eval("-std::f64::consts::PI"), Some(-pi));
        // A handful of others to confirm the table.
        assert_eq!(eval("std::f64::consts::TAU"), Some(std::f64::consts::TAU));
        assert_eq!(
            eval("std::f64::consts::SQRT_2"),
            Some(std::f64::consts::SQRT_2)
        );
        assert_eq!(
            eval("std::f64::consts::FRAC_PI_2"),
            Some(std::f64::consts::FRAC_PI_2)
        );
        assert_eq!(eval("std::f64::consts::LN_2"), Some(std::f64::consts::LN_2));
    }

    #[test]
    fn rejected_shapes() {
        // Unknown const ident under an accepted prefix.
        assert_eq!(eval("std::f64::consts::DOES_NOT_EXIST"), None);
        // Bare ident: ambiguous with crate-local consts, so not accepted.
        assert_eq!(eval("PI"), None);
        // Arbitrary crate path.
        assert_eq!(eval("crate::FOO"), None);
        // `f32::consts::*` is out of scope - the macro embeds `f64`.
        assert_eq!(eval("std::f32::consts::PI"), None);
        // Function calls and arithmetic expressions are not const-evaluated.
        assert_eq!(eval("some_fn()"), None);
        assert_eq!(eval("1 + 2"), None);
    }

    #[test]
    fn non_finite_range_args_produce_compile_error() {
        for bad in [
            "linear(NaN, 1.0)",
            "linear(-inf, 1.0)",
            "linear(0.0, inf)",
            "log(1.0, inf)",
            "offset_log(0.0, inf, 1.0)",
            "offset_log(0.0, 1.0, inf)",
            "pivoted_log(1.0, inf, 2.0)",
            "skewed(0.0, 1.0, inf)",
            "skewed(0.0, inf, 1.0)",
            "sym_skewed(0.0, 1.0, inf, 0.5)",
            "sym_skewed(0.0, 1.0, 1.0, NaN)",
        ] {
            let rendered = parse_range_tokens(bad).to_string();
            assert!(
                rendered.contains("compile_error"),
                "expected compile_error for `{bad}`, got `{rendered}`"
            );
        }
    }

    #[test]
    fn range_bounds_cover_offset_and_pivoted_log() {
        assert_eq!(
            range_bounds("offset_log(0.0, 20000.0, 10.0)"),
            Some((0.0, 20000.0))
        );
        assert_eq!(
            range_bounds("pivoted_log(20.0, 20000.0, 1000.0)"),
            Some((20.0, 20000.0))
        );
        assert_eq!(
            range_bounds("reversed(pivoted_log(20.0, 20000.0, 1000.0))"),
            Some((20.0, 20000.0))
        );
        assert_eq!(range_bounds("linear(-inf, 1.0)"), None);
    }

    #[test]
    fn default_range_error_checks_const_paths_and_log_variants() {
        use crate::fields::ParamAttrs;
        let make_field = |range: &str, default: Option<DefaultExpr>| ParamField {
            ident: syn::Ident::new("p", proc_macro2::Span::call_site()),
            kind: ParamKind::Float,
            enum_type: None,
            index: None,
            attrs: ParamAttrs {
                name: Some("P".into()),
                range: Some(range.into()),
                default,
                ..ParamAttrs::default()
            },
        };
        let mut nonfinite = make_field(
            "linear(0, 1)",
            Some(DefaultExpr {
                value: f64::INFINITY,
            }),
        );
        nonfinite.attrs.id = Some(0);
        assert!(gen_param_info_literal(&nonfinite).is_none());
        assert!(
            gen_field_constructor(&nonfinite, 0)
                .to_string()
                .contains("must be finite")
        );
        for range in [
            "offset_log(1e308, 1.5e308, 1e308)",
            "offset_log(1, 1.5e308, 1e308)",
        ] {
            assert!(
                parse_log_range(range)
                    .unwrap()
                    .to_string()
                    .contains("compile_error")
            );
        }
        let pi_path = DefaultExpr {
            value: std::f64::consts::PI,
        };
        assert!(default_range_error(&make_field("linear(0.0, 1.0)", Some(pi_path))).is_some());
        let ok_const = DefaultExpr {
            value: std::f64::consts::FRAC_1_SQRT_2,
        };
        assert!(default_range_error(&make_field("linear(0.0, 1.0)", Some(ok_const))).is_none());
        // Omitted default (0.0) is outside pivoted_log(20, 20000, 1000)
        assert!(
            default_range_error(&make_field("pivoted_log(20.0, 20000.0, 1000.0)", None)).is_some()
        );
        // Out-of-bounds default on offset_log
        let bad_off = DefaultExpr { value: -5.0 };
        assert!(
            default_range_error(&make_field("offset_log(0.0, 100.0, 1.0)", Some(bad_off)))
                .is_some()
        );
    }
}
