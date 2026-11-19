//! `plugin_info!` and friends: read `moose.toml` into a `PluginInfo` literal.

use moose_build::{Config, PluginDef};
use proc_macro::TokenStream;
use quote::quote;

/// Resolve `moose.toml` and pull out the `[[plugin]]` entry for the
/// current crate. Routes every failure mode through `Result<…, String>`
/// so callers can convert errors into `compile_error!` tokens with a
/// span - `panic!`-ing from a proc macro produces a span-less,
/// multi-line error frame instead of the clean compiler diagnostic the
/// caller actually wants.
pub(crate) fn try_resolve_plugin() -> Result<(Config, String, std::path::PathBuf), String> {
    let path = moose_build::find_moose_toml()?;
    let config = moose_build::load_config(&path)?;

    let pkg_name =
        std::env::var("CARGO_PKG_NAME").map_err(|_| "CARGO_PKG_NAME not set".to_string())?;
    if !config.plugin.iter().any(|p| p.serves(&pkg_name)) {
        let available: Vec<_> = config
            .plugin
            .iter()
            .map(|p| p.crate_name.as_str())
            .collect();
        return Err(format!(
            "No [[plugin]] entry with crate = \"{pkg_name}\" in {}. \
             Available: {}",
            path.display(),
            available.join(", ")
        ));
    }
    // Canonicalize so the embedded `include_bytes!` reference is stable
    // across `cargo check` / `cargo build` invocations (both resolve
    // CARGO_MANIFEST_DIR identically, but a future caller might run the
    // proc-macro from a symlinked path - canonicalizing pins the literal
    // to the realpath).
    let canonical = path.canonicalize().unwrap_or(path);
    Ok((config, pkg_name, canonical))
}

pub(crate) fn find_plugin<'a>(config: &'a Config, pkg_name: &str) -> &'a PluginDef {
    // try_resolve_plugin already verified the entry exists.
    config
        .plugin
        .iter()
        .find(|p| p.serves(pkg_name))
        .expect("try_resolve_plugin verified this entry exists")
}

/// Resolve the plugin's MIDI wiring and per-direction 2.0 dialects,
/// or the `compile_error!` tokens for a contradictory moose.toml
/// (capability vs port count, or a `midi2*` opt-in on a port-less
/// direction).
pub(crate) fn resolve_midi(
    plugin: &moose_build::PluginDef,
) -> Result<(moose_build::MidiWiring, bool, bool), TokenStream> {
    let to_tokens = |msg: String| -> TokenStream {
        syn::Error::new(proc_macro2::Span::call_site(), msg)
            .to_compile_error()
            .into()
    };
    let wiring = moose_build::midi_wiring(
        &plugin.category,
        plugin.midi_input,
        plugin.midi_output,
        plugin.midi_input_ports,
        plugin.midi_output_ports,
    )
    .map_err(to_tokens)?;
    let (midi2_in, midi2_out) = moose_build::midi2_dialects(
        &wiring,
        plugin.midi2,
        plugin.midi2_input,
        plugin.midi2_output,
    )
    .map_err(to_tokens)?;
    Ok((wiring, midi2_in, midi2_out))
}

/// Validate 4-ASCII-byte fourccs for `plugin_info!`. `Some(message)` on
/// the first bad code. `info::fourcc` asserts this at runtime (during the
/// host scan), so an invalid code otherwise compiles clean and the plugin
/// never loads in any DAW.
pub(crate) fn fourcc_error(codes: &[(&str, &str)]) -> Option<String> {
    for (label, code) in codes {
        if code.len() != 4 || !code.is_ascii() {
            return Some(format!(
                "`{label}` must be exactly 4 ASCII characters (an AU fourcc); \
                 got {code:?} ({} bytes)",
                code.len(),
            ));
        }
    }
    None
}

// `au_name` / `au3_name` mirror the user-facing moose.toml keys;
// renaming would break the 1:1 with the TOML schema.
// Linear metadata assembly: one `let` per moose.toml field feeding a
// single `PluginInfo` literal - splitting it would just thread the same
// locals through helpers without aiding clarity.
#[allow(clippy::similar_names, clippy::too_many_lines)]
pub(crate) fn plugin_info() -> TokenStream {
    let (config, pkg_name, moose_toml_path) = match try_resolve_plugin() {
        Ok(v) => v,
        Err(msg) => {
            // Surface the problem as a compile_error with a span at
            // the macro call-site instead of an opaque proc-macro
            // `panic!` (which renders as `error: proc macro panicked`
            // followed by a multi-line backtrace and no source span).
            return syn::Error::new(proc_macro2::Span::call_site(), msg)
                .to_compile_error()
                .into();
        }
    };
    let plugin = find_plugin(&config, &pkg_name);

    let name = &plugin.name;
    let bundle_id = &plugin.bundle_id;
    // The identity key feeds `plugin_id` (host-facing ids) and every
    // bundle/install path; a malformed one is a compile error.
    if let Err(msg) = moose_build::validate_bundle_id(bundle_id) {
        return syn::Error::new(proc_macro2::Span::call_site(), msg)
            .to_compile_error()
            .into();
    }
    let vendor = &config.vendor.name;
    let url = &config.vendor.url;
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".into());
    let version = plugin
        .version
        .as_deref()
        .unwrap_or(&pkg_version)
        .to_string();

    // Category-string vocabulary parsed by every consumer that reads
    // `moose.toml`. Note-effect plugins (`midi` / `note_effect`) map to
    // a distinct variant from `Effect`.
    let category = match plugin.category.as_str() {
        "instrument" => quote! { ::moose::core::PluginCategory::Instrument },
        "midi" | "note_effect" => quote! { ::moose::core::PluginCategory::NoteEffect },
        "analyzer" => quote! { ::moose::core::PluginCategory::Analyzer },
        "tool" => quote! { ::moose::core::PluginCategory::Tool },
        _ => quote! { ::moose::core::PluginCategory::Effect },
    };
    // MIDI wiring + dialects resolved together (capability is
    // `count > 0`; a port-less direction can't be 2.0) so every format
    // wrapper reads one coherent declaration. A contradictory
    // moose.toml is a compile error.
    let (wiring, midi2_in, midi2_out) = match resolve_midi(plugin) {
        Ok(v) => v,
        Err(tokens) => return tokens,
    };
    let (accepts_midi_in, emits_midi) = (wiring.accepts_midi_in, wiring.emits_midi);
    let (midi_input_ports, midi_output_ports) = (wiring.input_ports, wiring.output_ports);
    let dialect_tokens = |on: bool| {
        if on {
            quote! { ::moose::core::info::MidiDialect::Midi2 }
        } else {
            quote! { ::moose::core::info::MidiDialect::Midi1 }
        }
    };
    let midi_input_dialect = dialect_tokens(midi2_in);
    let midi_output_dialect = dialect_tokens(midi2_out);
    // NoteEffect plugins map to `aumi` (Apple's MIDI Processor type).
    // Pairs with empty `bus_layouts` at the plugin level: aumi
    // plugins must not expose audio I/O. Logic routes `aumi` to the
    // MIDI FX slot, which is where arpeggiators / transposers /
    // note-shapers belong. A mismatch with the AU-type computed at
    // install / package time causes auval to report "Class Data
    // fields ... do not match component description".
    // An audio effect that accepts MIDI input is an `aumf` MusicEffect,
    // not a plain `aufx`: AU routes MIDI to a plugin by its component
    // type, so an `aufx` would never be handed the events.
    let au_type = plugin
        .au_type
        .as_deref()
        .unwrap_or(match plugin.category.as_str() {
            "instrument" => "aumu",
            "midi" | "note_effect" => "aumi",
            _ if accepts_midi_in => "aumf",
            _ => "aufx",
        });

    let plugin_id = moose_build::plugin_id(&config.vendor.id, &plugin.bundle_id);

    let Some(resolved_fourcc) = plugin.fourcc.as_ref().or(plugin.au_subtype.as_ref()) else {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            format!(
                "moose.toml: [[plugin]] entry `{}` requires `fourcc` or `au_subtype`",
                plugin.crate_name
            ),
        )
        .to_compile_error()
        .into();
    };
    let au_manufacturer = &config.vendor.au_manufacturer;

    let vst3_subcategory = if let Some(sub) = &plugin.vst3_subcategory {
        quote! { Some(#sub) }
    } else {
        quote! { None }
    };

    // Per-format display-name overrides. Empty strings are normalized
    // to `None` here so format wrappers don't need to repeat the
    // empty-vs-unset distinction at every call site.
    let opt_str = |s: &Option<String>| -> proc_macro2::TokenStream {
        match s.as_deref() {
            Some(v) if !v.is_empty() => quote! { Some(#v) },
            _ => quote! { None },
        }
    };
    let preset_user_dir = opt_str(&plugin.presets.as_ref().and_then(|c| c.user_dir.clone()));
    let description = opt_str(&plugin.description);
    let clap_manual_url = opt_str(&plugin.clap_manual_url);
    let clap_support_url = opt_str(&plugin.clap_support_url);
    let vst3_name = opt_str(&plugin.vst3_name);
    let clap_name = opt_str(&plugin.clap_name);
    let au_name = opt_str(&plugin.au_name);
    let au3_name = opt_str(&plugin.au3_name);
    let clap_features = &plugin.clap_features;
    let clap_features = quote! { &[#(#clap_features),*] };
    let preset_extension = moose_build::preset_extension(plugin);
    if !preset_extension
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        let msg = format!(
            "moose.toml: [plugin.presets] extension {preset_extension:?} must be ASCII \
             letters, digits, `_` or `-` (no dot)"
        );
        return quote! { compile_error!(#msg); }.into();
    }
    let mute_preview_output = plugin.mute_preview_output;
    let automation = match config.automation.mode {
        moose_build::AutomationMode::PerSample => {
            quote! { ::moose::core::info::Automation::PerSample }
        }
        moose_build::AutomationMode::BlockSplit => {
            let min = config.automation.min_subblock_samples;
            quote! { ::moose::core::info::Automation::BlockSplit { min: #min } }
        }
    };

    // `[plugin.legacy_state]` AU probe keys, baked as a static slice.
    let legacy_au_keys = plugin
        .legacy_state
        .as_ref()
        .map_or(&[][..], |l| &l.au_keys[..]);
    let legacy_au_keys = quote! { &[#(#legacy_au_keys),*] };

    // `include_bytes!` registers `moose.toml` as a build-time dependency
    // through the compiler's normal dep-info tracking. Without it, edits
    // to moose.toml don't trigger a rebuild - proc macros on stable Rust
    // have no other way to declare external file dependencies.
    // Path is canonicalized in `try_resolve_plugin` so the literal is
    // stable across invocations.
    // `info::fourcc` asserts a 4-byte code at runtime - which fires when
    // the host first scans the plugin, so a 3-char or non-ASCII code
    // compiles clean and just never loads in any DAW. These are string
    // literals here, so validate at expansion time.
    if let Some(msg) = fourcc_error(&[
        ("fourcc / au_subtype", resolved_fourcc.as_str()),
        ("au_type", au_type),
        ("vendor au_manufacturer", au_manufacturer.as_str()),
    ]) {
        return quote! { compile_error!(#msg); }.into();
    }

    let moose_toml_lit = moose_toml_path.to_string_lossy().into_owned();
    let expanded = quote! {
        {
            const _MOOSE_TOML_DEP: &[u8] = include_bytes!(#moose_toml_lit);
            ::moose::core::PluginInfo {
                name: #name,
                vendor: #vendor,
                url: #url,
                version: #version,
                category: #category,
                description: #description,
                clap_manual_url: #clap_manual_url,
                clap_support_url: #clap_support_url,
                clap_features: #clap_features,
                accepts_midi_in: #accepts_midi_in,
                emits_midi: #emits_midi,
                midi_input_dialect: #midi_input_dialect,
                midi_output_dialect: #midi_output_dialect,
                midi_input_ports: #midi_input_ports,
                midi_output_ports: #midi_output_ports,
                bundle_id: #bundle_id,
                vst3_id: #plugin_id,
                clap_id: #plugin_id,
                fourcc: ::moose::core::info::fourcc(#resolved_fourcc.as_bytes()),
                au_type: ::moose::core::info::fourcc(#au_type.as_bytes()),
                au_manufacturer: ::moose::core::info::fourcc(#au_manufacturer.as_bytes()),
                vst3_subcategory: #vst3_subcategory,
                preset_user_dir: #preset_user_dir,
                preset_extension: #preset_extension,
                vst3_name: #vst3_name,
                clap_name: #clap_name,
                au_name: #au_name,
                au3_name: #au3_name,
                mute_preview_output: #mute_preview_output,
                automation: #automation,
                legacy_au_keys: #legacy_au_keys,
            }
        }
    };

    expanded.into()
}

pub(crate) fn plugin_vst3_class_id() -> TokenStream {
    let (config, pkg_name, moose_toml_path) = match try_resolve_plugin() {
        Ok(value) => value,
        Err(msg) => {
            return syn::Error::new(proc_macro2::Span::call_site(), msg)
                .to_compile_error()
                .into();
        }
    };
    let plugin_crate = &find_plugin(&config, &pkg_name).crate_name;
    match moose_build::load_vst3_class_ids(&moose_toml_path) {
        Ok(ids) => match ids
            .into_iter()
            .find(|(crate_name, _)| crate_name == plugin_crate)
        {
            Some((_, bytes)) => quote! { Some([#(#bytes),*]) }.into(),
            None => quote! { None }.into(),
        },
        Err(msg) => syn::Error::new(proc_macro2::Span::call_site(), msg)
            .to_compile_error()
            .into(),
    }
}
