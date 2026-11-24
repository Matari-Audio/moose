//! Per-format cdylib build helper, shared by `cargo moose build` and
//! `cargo moose install`.
//!
//! Both commands run an identical sequence for every selected format:
//!
//! 1. Skip on unsupported platforms (AU is macOS-only) with a single
//!    `log_skip` line.
//! 2. One `cargo build -p a -p b -p c …` per format batch with the
//!    format's feature set. Per-format display names travel with
//!    `PluginInfo` (baked by `moose::plugin_info!`). AU v2 is the
//!    exception: one build per plugin with `MOOSE_AU_PLUGIN_ID` set, so
//!    each dylib's Cocoa view class gets a unique name.
//! 3. Copy the produced `lib<stem>.<dylib-ext>` to a format-suffixed
//!    path (`<stem>_clap`, `<stem>_vst3`, `<stem>_au`) so the next format build
//!    doesn't overwrite the previous one (every plugin's cdylib lands
//!    at the same canonical cargo path).

use crate::util::fs_ctx;
use crate::{CargoMooseError, PluginDef, Res, cargo_build, release_lib_for_target};
use moose_utils::shell_sidecar::sidecar_path;
use std::path::Path;

/// Required dynamic symbols exported by Linux CLAP plugin cdylibs.
#[cfg(any(target_os = "linux", test))]
const LINUX_CLAP_EXPORTS: &[&str] = &["clap_entry"];

/// Required dynamic symbols exported by Linux VST3 plugin cdylibs.
#[cfg(any(target_os = "linux", test))]
const LINUX_VST3_EXPORTS: &[&str] = &["GetPluginFactory", "ModuleEntry", "ModuleExit"];

/// Standard linker-injected ELF symbols permitted in Linux cdylib `.dynsym`.
#[cfg(any(target_os = "linux", test))]
const ELF_LINKER_SYMBOLS: &[&str] = &["_init", "_fini", "__bss_start", "_edata", "_end"];

/// One of the per-format cdylib targets the build / install pipelines
/// produce. Encodes the cargo feature flag, the format-suffix used in
/// the workspace target dir, and the platform-gate behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuildFormat {
    Clap,
    Vst3,
    Au2,
    Aax,
}

impl BuildFormat {
    /// Cargo feature flag passed to `cargo build --features <feature>`.
    pub(crate) fn feature(self) -> &'static str {
        match self {
            BuildFormat::Clap => "clap",
            BuildFormat::Vst3 => "vst3",
            BuildFormat::Au2 => "au",
            BuildFormat::Aax => "aax",
        }
    }

    /// Human-facing name used in the `Building <label>...` banner.
    pub(crate) fn label(self) -> &'static str {
        match self {
            BuildFormat::Clap => "CLAP",
            BuildFormat::Vst3 => "VST3",
            BuildFormat::Au2 => "AU v2",
            BuildFormat::Aax => "AAX",
        }
    }

    /// Format-suffix appended to the dylib stem on copy. Keeps each
    /// format's binary distinct in `target/<profile>/` so subsequent
    /// per-format builds don't overwrite earlier ones.
    pub(crate) fn dylib_suffix(self) -> &'static str {
        match self {
            BuildFormat::Clap => "_clap",
            BuildFormat::Vst3 => "_vst3",
            BuildFormat::Au2 => "_au",
            BuildFormat::Aax => "_aax",
        }
    }
}

/// Build cdylibs for one format across `plugins`. Centralizes the
/// per-format banner, env-var assembly, cargo build, and copy-to-suffix
/// step that `cargo moose build` and `cargo moose install` share.
///
/// Platform gate: `Au2` is macOS only. Other platforms emit
/// `crate::log_skip` and return `Ok(())` so callers don't need cfg
/// blocks at the call site.
///
/// `extra_features` are appended to the format's own feature (used by
/// shell-mode builds to add `"shell"`); empty otherwise.
///
/// `target` selects the cargo `--target <triple>`; `None` means "build
/// for the host" and outputs land at `target/release/`. With a target
/// set, outputs land at `target/<triple>/release/` and downstream
/// stagers must read from there too.
pub(crate) fn build_format_dylibs(
    format: BuildFormat,
    plugins: &[&PluginDef],
    extra_features: &[&str],
    root: &Path,
    deployment_target: &str,
    target: Option<&str>,
) -> Res {
    // Platform gate first: emit a single skip line and exit cleanly, so
    // the caller's "if format_selected { build }" needs no cfg arms.
    #[cfg(not(target_os = "macos"))]
    if format == BuildFormat::Au2 {
        crate::log_skip(
            "AU v2: not supported on this platform. Audio Unit is macOS-only.".to_string(),
        );
        return Ok(());
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    if format == BuildFormat::Aax {
        crate::log_skip(
            "AAX: not supported on this platform. Pro Tools AAX is macOS/Windows only.".to_string(),
        );
        return Ok(());
    }

    // Build banner. Shell-mode label gets the extra-feature list
    // parenthesised (e.g. "Building CLAP (shell)...").
    if extra_features.is_empty() {
        crate::vprintln!("Building {}...", format.label());
    } else {
        let extras = extra_features.join(" + ");
        crate::vprintln!("Building {} ({extras})...", format.label());
    }

    // Base features for every build in this format: the format itself plus
    // any caller extras (`shell`). Each build also re-adds its plugins'
    // non-format default features (e.g. `ara`) that `--no-default-features`
    // would otherwise strip - namespaced per plugin, so it's appended per
    // cargo invocation below.
    let mut base_features: Vec<String> = vec![format.feature().to_string()];
    base_features.extend(extra_features.iter().map(|s| (*s).to_string()));
    let features_with = |crate_names: &[&str]| -> String {
        let mut feats = base_features.clone();
        feats.extend(crate::namespaced_nonformat_defaults(root, crate_names));
        feats.join(",")
    };

    // AU v2 needs a per-plugin `MOOSE_AU_PLUGIN_ID` env so each
    // dylib's cocoa-view class lands in `__objc_classlist` under a
    // unique name. Hosts load every `.component` into one process;
    // libobjc dedupes classes by name and `[NSBundle classNamed:]`
    // returns nil on the loser's bundle - host then thinks the
    // plugin has no GUI. Splitting AU2 into one cargo invocation per
    // plugin is the cost of correctness here; moose-au's tiny C/ObjC
    // shim recompiles per plugin but the leaf cdylib link cost
    // dominates anyway.
    if format == BuildFormat::Au2 {
        for p in plugins {
            let env_pairs: &[(&str, &str)] = &[("MOOSE_AU_PLUGIN_ID", p.bundle_id.as_str())];
            let mut cargo_args: Vec<String> = Vec::with_capacity(7);
            cargo_args.push("-p".into());
            cargo_args.push(p.crate_name.clone());
            cargo_args.push("--no-default-features".into());
            cargo_args.push("--features".into());
            cargo_args.push(features_with(&[p.crate_name.as_str()]));
            if let Some(t) = target {
                cargo_args.push("--target".into());
                cargo_args.push(t.into());
            }
            let cargo_arg_refs: Vec<&str> = cargo_args.iter().map(String::as_str).collect();
            cargo_build(env_pairs, &cargo_arg_refs, deployment_target)?;
        }
    } else {
        let env_pairs: &[(&str, &str)] = &[];
        let mut cargo_args: Vec<String> = Vec::with_capacity(plugins.len() * 2 + 5);
        for p in plugins {
            cargo_args.push("-p".into());
            cargo_args.push(p.crate_name.clone());
        }
        cargo_args.push("--no-default-features".into());
        cargo_args.push("--features".into());
        let names: Vec<&str> = plugins.iter().map(|p| p.crate_name.as_str()).collect();
        cargo_args.push(features_with(&names));
        if let Some(t) = target {
            cargo_args.push("--target".into());
            cargo_args.push(t.into());
        }
        let cargo_arg_refs: Vec<&str> = cargo_args.iter().map(String::as_str).collect();
        cargo_build(env_pairs, &cargo_arg_refs, deployment_target)?;
    }

    // Post-build per-plugin staging: copy the produced `.dylib` to
    // its format-suffixed name. Cheap I/O, kept as a separate pass so
    // the cargo invocation above doesn't have to know about it.
    for p in plugins {
        let src = release_lib_for_target(root, &p.dylib_stem(), target);
        let dst = release_lib_for_target(
            root,
            &format!("{}{}", p.dylib_stem(), format.dylib_suffix()),
            target,
        );
        // CLAP / VST3 historically guarded the copy with `if src.exists()`
        // because a feature-flagged plugin can legitimately produce no
        // output for a format it doesn't support; preserve that for
        // every format so the loop is uniformly tolerant.
        if src.exists() {
            fs_ctx::copy(&src, &dst)?;
            #[cfg(target_os = "linux")]
            {
                let triple = target.unwrap_or_else(|| moose_build::host_triple());
                if matches!(format, BuildFormat::Clap | BuildFormat::Vst3)
                    && crate::target_os_of(triple) == "linux"
                {
                    verify_linux_cdylib_exports(&dst, triple, format, &p.linux_extra_exports)?;
                }
            }
        }

        // Static-linked AAX consumes the Rust `staticlib` (`lib<stem>.a` /
        // `<stem>.lib`); copy it to `<stem>_aax` so subsequent format builds
        // cannot overwrite the AAX archive.
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if format == BuildFormat::Aax {
            let static_src = crate::release_static_for_target(root, &p.dylib_stem(), target);
            let static_dst = crate::release_static_for_target(
                root,
                &format!("{}{}", p.dylib_stem(), format.dylib_suffix()),
                target,
            );
            if static_src.exists() {
                fs_ctx::copy(&static_src, &static_dst)?;
            }
        }

        // macOS bundle-format link path. The cdylib above is MH_DYLIB,
        // which CFBundle rejects on the JUCE-hosted VST3 path. Run
        // `clang -bundle` against the matching Rust `staticlib` to
        // produce a real MH_BUNDLE at the canonical bundle-bin path
        // that stage / install steps read from. AU2 keeps the
        // cdylib (their loaders are happy with MH_DYLIB).
        // Only for a macOS *target* - cross-compiling to Windows / Linux
        // keeps the cdylib, and `clang -bundle` can't relink a foreign-arch
        // static archive anyway.
        #[cfg(target_os = "macos")]
        if matches!(format, BuildFormat::Clap | BuildFormat::Vst3)
            && crate::target_os_of(target.unwrap_or_else(|| moose_build::host_triple())) == "macos"
        {
            link_macos_bundle_for_plugin(root, p, format, target)?;
        }
    }

    Ok(())
}

/// Build the per-plugin "logic" dylib (the cdylib the shell-mode shell
/// dlopens at runtime). Profile is `release` by default; `--debug`
/// flips it to cargo's debug profile; custom profiles fall through to
/// `cargo build --profile <name>`. Scoped per-plugin so a fresh
/// checkout doesn't rebuild every framework crate.
///
/// After every successful build, writes the sidecar that the shell
/// binary reads at runtime to find this dylib - see
/// [`write_shell_sidecar`].
pub(crate) fn build_logic_dylibs(
    plugins: &[&PluginDef],
    logic_profile: &str,
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] deployment_target: &str,
) -> Res {
    use std::process::Command;

    let root = crate::project_root();
    for p in plugins {
        crate::vprintln!(
            "Building {} logic dylib for {}...",
            logic_profile,
            p.crate_name
        );
        let mut cmd = Command::new("cargo");
        // No format feature: the hot-reload exports are compiled only
        // without one (they stay out of release format builds). Keep the
        // plugin's non-format defaults so the ABI canary matches the shell.
        cmd.arg("build")
            .arg("-p")
            .arg(&p.crate_name)
            .arg("--no-default-features")
            .arg("--lib");
        let defaults = crate::namespaced_nonformat_defaults(&root, &[p.crate_name.as_str()]);
        if !defaults.is_empty() {
            cmd.arg("--features").arg(defaults.join(","));
        }
        match logic_profile {
            "debug" => {} // cargo default
            "release" => {
                cmd.arg("--release");
            }
            other => {
                cmd.arg("--profile").arg(other);
            }
        }
        #[cfg(target_os = "macos")]
        cmd.env("MACOSX_DEPLOYMENT_TARGET", deployment_target);
        if let Some(wrapper) = crate::util::sccache_wrapper() {
            cmd.env("RUSTC_WRAPPER", wrapper);
        }
        // The shell-mode logic dylib is a separate build from the shell;
        // it must carry the same extra features or the ABI canary that
        // gates the dlopen mismatches (feature-dependent ABI like sample
        // precision). Applied here, not via a param, so it can't be
        // forgotten while the format build gets features.
        crate::apply_extra_features(&mut cmd);
        let status = cmd.status()?;
        if !status.success() {
            return Err(format!("{logic_profile} build of {} failed", p.crate_name).into());
        }

        write_shell_sidecar(&root, &p.crate_name, logic_profile)?;
    }
    Ok(())
}

/// Resolve and write `~/.moose/shell/<crate>.path` so the installed
/// shell binary (loaded by the DAW) can find the logic dylib at
/// runtime. Writes the absolute, canonicalized path of the logic
/// dylib so the runtime read site doesn't have to re-resolve
/// `CARGO_TARGET_DIR` / `[build].target-dir` from a context that
/// lacks those signals.
///
/// Atomic write: lands the contents at a `<sidecar>.tmp.<pid>`
/// sibling and renames it into place. A `^C` or power loss between
/// the temp write and the rename leaves the prior sidecar intact;
/// the half-written temp file is harmless and gets overwritten on
/// the next build.
fn write_shell_sidecar(root: &std::path::Path, crate_name: &str, logic_profile: &str) -> Res {
    use std::fs;

    let stem = crate_name.replace('-', "_");
    let dylib_path = moose_build::target_dir(root)
        .join(logic_profile)
        .join(crate::util::shared_lib_name(&stem));
    let canonical = dylib_path.canonicalize().unwrap_or(dylib_path);

    let sidecar = sidecar_path(crate_name).ok_or_else(|| -> crate::CargoMooseError {
        "could not resolve $HOME (or %USERPROFILE% on Windows) for the \
         shell sidecar - the runtime needs $HOME to locate the logic \
         dylib without it"
            .into()
    })?;
    if let Some(parent) = sidecar.parent() {
        fs::create_dir_all(parent).map_err(|e| -> crate::CargoMooseError {
            format!("failed to create {}: {e}", parent.display()).into()
        })?;
    }
    let tmp = sidecar.with_extension(format!("path.tmp.{}", std::process::id()));
    fs::write(&tmp, format!("{}\n", canonical.display())).map_err(
        |e| -> crate::CargoMooseError {
            format!("failed to write shell sidecar {}: {e}", tmp.display()).into()
        },
    )?;
    // `fs::rename` is atomic on POSIX (rename(2)) and on Windows
    // (`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`). Same parent
    // directory guarantees same filesystem.
    fs::rename(&tmp, &sidecar).map_err(|e| -> crate::CargoMooseError {
        let _ = fs::remove_file(&tmp);
        format!(
            "failed to rename {} -> {}: {e}",
            tmp.display(),
            sidecar.display()
        )
        .into()
    })?;
    crate::vprintln!(
        "Wrote shell sidecar {} -> {}",
        sidecar.display(),
        canonical.display(),
    );
    Ok(())
}

/// Link a per-arch Rust staticlib into a macOS loadable bundle binary
/// and place it at the canonical `target/<profile>/<stem>_<fmt>.bundle-bin`
/// path. Idempotent over the input archs: each call replaces the
/// previous bundle binary at that path.
///
/// `target` selects which `target/<triple>/release/` to read the
/// `.a` from; `None` uses the host's `target/release/` (single-arch
/// install / `cargo moose build` host-only path).
#[cfg(target_os = "macos")]
fn link_macos_bundle_for_plugin(
    root: &Path,
    p: &PluginDef,
    format: BuildFormat,
    target: Option<&str>,
) -> Res {
    let staticlib = crate::release_static_for_target(root, &p.dylib_stem(), target);
    if !staticlib.exists() {
        return Err(crate::missing_staticlib_error(&staticlib).into());
    }

    // For host-only builds the cargo profile dir is `target/release/`;
    // we infer the arch from cfg!(target_arch) for that case. For
    // explicit `--target` builds we read the triple's arch prefix.
    let arch = match target {
        Some(t) if t.starts_with("aarch64") => crate::MacArch::Arm64,
        Some(t) if t.starts_with("x86_64") => crate::MacArch::X86_64,
        Some(other) => {
            return Err(
                format!("unrecognized cargo target triple {other} for macOS bundle link").into(),
            );
        }
        None => crate::MacArch::host(),
    };

    let exports = match format {
        BuildFormat::Clap => crate::CLAP_EXPORTS,
        BuildFormat::Vst3 => crate::VST3_EXPORTS,
        BuildFormat::Au2 | BuildFormat::Aax => unreachable!("caller gates on bundle formats"),
    };

    let out = crate::release_bundle_bin(root, &p.dylib_stem(), format.dylib_suffix());
    crate::link_macos_bundle(
        &[(arch, staticlib)],
        exports,
        &crate::deployment_target(),
        &out,
    )?;
    Ok(())
}

/// Verify that a Linux CLAP or VST3 cdylib exports ONLY the expected format
/// entry points (plus any declared `linux_extra_exports` and standard ELF
/// linker symbols) via `nm -D --defined-only`, using an `nm` that reads the
/// target's ELF (see [`nm_candidates`]).
///
/// `rustc` automatically emits an anonymous `--version-script` for `cdylib`
/// targets that hides all internal Rust/dependency symbols, leaving only
/// `#[unsafe(no_mangle)]` / `#[export_name]` symbols in `.dynsym`. Checking
/// the resulting `.so` directly catches any accidental symbol leak without a
/// custom linker wrapper.
#[cfg(target_os = "linux")]
fn verify_linux_cdylib_exports(
    so_path: &Path,
    target: &str,
    format: BuildFormat,
    extra_exports: &[String],
) -> Res {
    use std::io::ErrorKind;
    use std::process::Command;

    let candidates = nm_candidates(target, moose_build::host_triple());
    let mut found = None;
    for nm in &candidates {
        match Command::new(nm)
            .args(["-D", "--defined-only"])
            .arg(so_path)
            .output()
        {
            Ok(out) => {
                found = Some((nm, out));
                break;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "failed to run `{nm} -D --defined-only {}`: {e}",
                    so_path.display()
                )
                .into());
            }
        }
    }
    let Some((nm, output)) = found else {
        crate::vprintln!(
            "Warning: none of {} found on PATH; skipping Linux export verification for {}",
            candidates.join(", "),
            so_path.display()
        );
        return Ok(());
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`{nm} -D --defined-only {}` failed ({}): {}",
            so_path.display(),
            output.status,
            stderr.trim()
        )
        .into());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols = parse_nm_defined_dynamic_symbols(&stdout);
    check_linux_cdylib_exports(&symbols, format, extra_exports).map_err(|msg| {
        format!(
            "Linux {} export check failed for {}: {msg}",
            format.label(),
            so_path.display()
        )
        .into()
    })
}

/// `nm` binaries able to read a `target` ELF, in preference order. A native
/// build uses the host `nm`. A cross build cannot: a host-only binutils `nm`
/// rejects the foreign object format, so it tries the target-agnostic
/// `llvm-nm`, then the cross binutils `<arch>-linux-<env>-nm`.
///
/// ponytail: the cross prefix drops the vendor field (`aarch64-unknown-linux-gnu`
/// -> `aarch64-linux-gnu`), which is Debian/Fedora's convention; add a
/// `moose.toml` override if a toolchain names it differently.
#[cfg(any(target_os = "linux", test))]
fn nm_candidates(target: &str, host: &str) -> Vec<String> {
    if target == host {
        return vec!["nm".into(), "llvm-nm".into()];
    }
    let cross_prefix = target.replacen("-unknown-", "-", 1);
    vec!["llvm-nm".into(), format!("{cross_prefix}-nm")]
}

/// Extract defined dynamic symbol names from `nm -D --defined-only` stdout,
/// stripping any ELF symbol version suffix (`sym@VER` / `sym@@VER`).
#[cfg(any(target_os = "linux", test))]
fn parse_nm_defined_dynamic_symbols(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let first = parts.next()?;
            let sym = match (parts.next(), parts.next()) {
                // Standard defined line: `<addr> <type> <name>`
                (Some(_kind), Some(name)) => name,
                // Fallback if address column is omitted: `<type> <name>`
                (Some(name), None) => name,
                _ => {
                    let _ = first;
                    return None;
                }
            };
            sym.split('@').next().filter(|s| !s.is_empty())
        })
        .collect()
}

/// Validate `symbols` against the required and permitted Linux dynamic exports
/// for `format`.
#[cfg(any(target_os = "linux", test))]
fn check_linux_cdylib_exports(
    symbols: &[&str],
    format: BuildFormat,
    extra_exports: &[String],
) -> Result<(), CargoMooseError> {
    let required: &[&str] = match format {
        BuildFormat::Clap => LINUX_CLAP_EXPORTS,
        BuildFormat::Vst3 => LINUX_VST3_EXPORTS,
        BuildFormat::Au2 | BuildFormat::Aax => return Ok(()),
    };

    for sym in extra_exports {
        if !is_valid_elf_export_symbol(sym) {
            return Err(format!(
                "invalid symbol name {sym:?} in linux_extra_exports (expected C identifier)"
            )
            .into());
        }
    }

    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|req| !symbols.contains(req))
        .collect();
    if !missing.is_empty() {
        return Err(format!("missing required dynamic exports: {}", missing.join(", ")).into());
    }

    let leaked: Vec<&str> = symbols
        .iter()
        .copied()
        .filter(|sym| {
            !required.contains(sym)
                && !ELF_LINKER_SYMBOLS.contains(sym)
                && !extra_exports.iter().any(|extra| extra == sym)
        })
        .collect();
    if !leaked.is_empty() {
        return Err(format!(
            "unexpected leaked dynamic symbols: {} (remove accidental #[no_mangle]/#[export_name] or declare intentional exports in moose.toml `linux_extra_exports`)",
            leaked.join(", ")
        )
        .into());
    }

    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn is_valid_elf_export_symbol(sym: &str) -> bool {
    let mut bytes = sym.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod linux_export_check_tests {
    use super::*;

    #[test]
    fn cross_target_export_check_never_uses_host_nm() {
        let host = "x86_64-unknown-linux-gnu";
        assert_eq!(nm_candidates(host, host), ["nm", "llvm-nm"]);
        assert_eq!(
            nm_candidates("aarch64-unknown-linux-gnu", host),
            ["llvm-nm", "aarch64-linux-gnu-nm"]
        );
    }

    #[test]
    fn clap_allows_clap_entry_and_elf_linker_symbols() {
        let stdout = "\
00000000001af030 D clap_entry
0000000000010000 T _init
0000000000020000 T _fini
";
        let syms = parse_nm_defined_dynamic_symbols(stdout);
        assert_eq!(syms, vec!["clap_entry", "_init", "_fini"]);
        check_linux_cdylib_exports(&syms, BuildFormat::Clap, &[]).unwrap();
    }

    #[test]
    fn vst3_allows_exact_entry_points() {
        let stdout = "\
0000000000406860 T GetPluginFactory
00000000004068b4 T ModuleEntry
00000000004068b8 T ModuleExit
";
        let syms = parse_nm_defined_dynamic_symbols(stdout);
        check_linux_cdylib_exports(&syms, BuildFormat::Vst3, &[]).unwrap();
    }

    #[test]
    fn rejects_leaked_and_missing_symbols_and_honors_extra_exports() {
        let leaked = vec!["clap_entry", "accidental_leak"];
        let err = check_linux_cdylib_exports(&leaked, BuildFormat::Clap, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("accidental_leak"), "unexpected error: {err}");

        check_linux_cdylib_exports(&leaked, BuildFormat::Clap, &["accidental_leak".into()])
            .unwrap();

        let missing = vec!["GetPluginFactory", "ModuleEntry"];
        let err = check_linux_cdylib_exports(&missing, BuildFormat::Vst3, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("ModuleExit"), "unexpected error: {err}");

        for bad in ["", "123bad", "bad;local:*", "bad space", "a/b", "*"] {
            assert!(
                check_linux_cdylib_exports(&["clap_entry"], BuildFormat::Clap, &[bad.into()])
                    .is_err(),
                "must reject {bad:?}"
            );
        }
    }
}
