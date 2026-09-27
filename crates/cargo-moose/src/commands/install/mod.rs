//! `cargo moose install` - build per-format dylibs and install into the
//! standard plug-in directories.

use crate::format::Format;
use crate::install_scope::{InstallScope, effective_scope, note_once, set_cli_install_scope};
use crate::util::{fs_ctx, parse_target_cpu_arg};
use crate::{
    Config, PluginDef, Res, deployment_target, detect_default_features, load_config, project_root,
};
// `run_sudo` shells out to `/usr/bin/sudo` and is therefore macOS-only.
// Windows admin elevation is per-process; the non-macOS install branches
// below use `fs_ctx` directly and surface the OS-level EACCES if the user
// isn't elevated for a system-scope write.
#[cfg(target_os = "macos")]
use crate::run_sudo;
// CLAP / VST3 read the cdylib from `release_lib` on non-macOS
// targets only; on macOS those formats consume the bundle-bin produced
// by the `clang -bundle` link step, so `release_lib` is unused there.
#[cfg(not(target_os = "macos"))]
use crate::release_lib;
// Plist scratch (CLAP / VST3) only happens on macOS - gate the
// import so Windows / Linux builds don't see it as unused.
#[cfg(target_os = "macos")]
use crate::codesign_bundle;
#[cfg(target_os = "macos")]
use crate::tmp_manifests;
// `OsStr` (run_sudo args) and `fs` (pre-install remove_dir) are only
// touched from macOS-gated branches below.
#[cfg(target_os = "macos")]
use std::ffi::OsStr;
#[cfg(target_os = "macos")]
use std::fs;
use std::path::Path;

pub(crate) mod presets;

use presets::FactoryPresets;

/// Guarantee the param-manifest sidecar exists for every plugin that
/// ships presets, so install-time preset name resolution can't fail on a
/// missing one.
///
/// `moose::plugin!` / `#[derive(Params)]` write
/// `target/param-index/<crate>/param_index.toml` as a compile-time side
/// effect, but cargo doesn't track it: delete it (or `cargo clean` only
/// that dir) while the crate stays cached and the next *incremental* build
/// won't re-run the macro, leaving preset install to abort with "no param
/// manifest". When the file is missing we force `cargo clean -p <crate>`
/// so the upcoming build recompiles the crate and regenerates it. A crate
/// that was never built has nothing to clean (the build writes the sidecar
/// naturally); only the deleted-but-cached case actually pays for it.
fn ensure_preset_sidecars(plugins: &[&PluginDef], root: &Path) -> Res {
    for p in plugins {
        let ships_presets = presets::authored_presets_dir(root, p).is_some_and(|d| d.is_dir());
        if !ships_presets {
            continue;
        }
        let sidecar = moose_build::param_index_dir(&moose_build::target_dir(root), &p.crate_name)
            .join("param_index.toml");
        if sidecar.exists() {
            continue;
        }
        crate::vprintln!(
            "  Param manifest for {} is missing; cleaning it so the build regenerates it.",
            p.crate_name
        );
        let status = std::process::Command::new("cargo")
            .arg("clean")
            .arg("-p")
            .arg(&p.crate_name)
            .status()
            .map_err(|e| format!("running `cargo clean -p {}`: {e}", p.crate_name))?;
        if !status.success() {
            return Err(format!(
                "`cargo clean -p {}` failed; needed to regenerate its preset param manifest",
                p.crate_name
            )
            .into());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_install(args: &[String]) -> Res {
    let config = load_config()?;

    let mut clap = false;
    let mut vst3 = false;
    let mut no_build = false;
    let mut shell_mode = false;
    let mut debug = false;
    let mut target_cpu_arg: Option<String> = None;
    let mut plugin_filter: Option<String> = None;
    let mut cli_scope: Option<InstallScope> = None;
    let mut user_features: Vec<String> = Vec::new();
    let mut no_default_features = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--clap" => clap = true,
            "--vst3" => vst3 = true,
            "--no-build" => no_build = true,
            "--shell" => shell_mode = true,
            "--debug" => debug = true,
            "--target-cpu" => {
                target_cpu_arg =
                    Some(crate::util::arg_value(args, &mut i, "--target-cpu")?.to_string());
            }
            "--user" => set_cli_install_scope(&mut cli_scope, InstallScope::User)?,
            "--system" => set_cli_install_scope(&mut cli_scope, InstallScope::System)?,
            "--ask" => {
                return Err(
                    "--ask is not valid for `cargo moose install` (no end user to prompt). \
                     Use --user or --system."
                        .into(),
                );
            }
            "-p" => {
                plugin_filter = Some(crate::util::arg_value(args, &mut i, "-p")?.to_string());
            }
            "--features" => {
                user_features.extend(crate::parse_extra_features(crate::util::arg_value(
                    args,
                    &mut i,
                    "--features",
                )?)?);
            }
            "--no-default-features" => no_default_features = true,
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
        i += 1;
    }

    // Extra Cargo features apply to every underlying build (desktop
    // formats, shell logic) via the global read by
    // `apply_extra_features`.
    crate::set_extra_features(user_features);
    // `--no-default-features` opts out of re-adding each plugin's
    // non-format default features (e.g. `ara`) to the per-format builds.
    crate::set_no_default_features(no_default_features);

    // Scope is resolved per-format inside `scope_for` / `effective_scope`:
    // - explicit `--user` / `--system` wins (subject to hard upgrades);
    // - no flag falls back to the per-(format, OS) default (user
    //   everywhere except VST3 on Windows, which defaults to system).

    // In shell mode, `--debug` selects the *logic* profile. The
    // shell binary itself is always built into `target/shell/` via a
    // custom cargo profile; the second build (the logic dylib the
    // shell dlopens at runtime) defaults to release for better DSP
    // perf, with `--debug` flipping it to debug for fast iteration.

    if !clap && !vst3 {
        // No format flags specified - enable all formats that the project supports.
        // Check which features are defined in the first plugin's Cargo.toml.
        let available = detect_default_features();
        clap = available.contains("clap");
        vst3 = available.contains("vst3");
    }

    // Shell-mode preflight: bail early if the user's Cargo.toml is
    // missing `[profile.shell]`. Catching this here gives a one-line
    // copy-paste fix instead of cargo's terser "profile `shell` is not
    // declared" downstream.
    if shell_mode {
        crate::verify_shell_profile_declared()?;
    }

    // Filter plugins if -p specified
    let plugins: Vec<&PluginDef> = super::pick_plugins(&config, plugin_filter.as_deref())?;

    // Profile of the logic dylib that the shell dlopens at runtime.
    // Only meaningful when `shell_mode` is on.
    let logic_profile = if debug { "debug" } else { "release" };

    // For shell-mode builds the per-format cargo invocation uses the
    // custom `[profile.shell]` (defined in the user's Cargo.toml,
    // inherits from "release"). Output lands in `target/shell/`,
    // independent of `target/release/` and `target/debug/`. For the
    // non-shell case we honor `--debug` directly.
    if shell_mode {
        crate::set_build_profile("shell");
    } else {
        crate::set_debug_profile(debug);
    }
    let target_cpu = target_cpu_arg
        .as_deref()
        .map(parse_target_cpu_arg)
        .unwrap_or_default();
    crate::set_target_cpu(target_cpu);

    let root = project_root();
    let dt = &deployment_target();

    let mut extra_features = Vec::new();
    if shell_mode {
        extra_features.push("shell");
    }

    // Preset name resolution reads a param-manifest sidecar the plugin
    // macro writes at compile time; force a rebuild of any preset-shipping
    // plugin whose sidecar has gone missing so the upcoming build restores
    // it. Skipped under `--no-build` (nothing would rebuild it).
    if !no_build {
        ensure_preset_sidecars(&plugins, &root)?;
    }

    // --- Build ---
    //
    // One cargo invocation per (plugin, format) pair so the
    // name-override env vars can be applied per-plugin. The
    // format-suffix copy lives inside `build_format_dylibs`; the
    // shared target cache absorbs the cost of one cargo invocation
    // per format.
    if !no_build {
        use super::build_dylibs::{BuildFormat, build_format_dylibs, build_logic_dylibs};
        let format_selection: &[(bool, BuildFormat)] =
            &[(clap, BuildFormat::Clap), (vst3, BuildFormat::Vst3)];
        for &(selected, format) in format_selection {
            if selected {
                build_format_dylibs(format, &plugins, &extra_features, &root, dt, None)?;
            }
        }

        // Shell mode: also build the logic dylibs the installed shells
        // dlopen at runtime. Profile follows `--debug` (release otherwise).
        if shell_mode {
            build_logic_dylibs(&plugins, logic_profile, dt)?;
        }
    }

    // --- Install ---
    //
    // Per-format scope is resolved through `effective_scope`, which
    // applies the per-(format, OS) default (when no CLI flag is set),
    // emitting a one-line note (printed at most once per message via
    // `note_once`).
    for p in &plugins {
        // Parsed once per plugin; each format re-envelopes the same
        // canonical state blobs into its native preset files.
        let factory_presets = presets::load_factory_presets(&root, p, &config)?;
        let fp = factory_presets.as_ref();
        if clap {
            let s = scope_for(Format::Clap, cli_scope);
            install_clap(&root, p, &config, s, fp)?;
        }
        if vst3 {
            let s = scope_for(Format::Vst3, cli_scope);
            install_vst3(&root, p, &config, s, fp)?;
        }
    }

    let installed = crate::take_outputs();
    if !installed.is_empty() {
        eprintln!("\nInstalled:");
        for line in installed {
            eprintln!("  {line}");
        }
    }
    eprintln!("\nDone. Restart your DAW to rescan.");
    Ok(())
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo moose install [--clap] [--vst3]
                           [--user|--system] [--shell] [--debug] [--no-build] [-p <crate>]
                           [--target-cpu <value>]

Build and install plugins into the host's plug-in directories. Defaults
to release. Defaults to whichever formats are in the plugin's Cargo.toml
default features (typically clap + vst3).

Per-format scope is per-user by default; pass --system for the shared
system directories. VST3 on
Windows defaults to system scope (the directory every commercial host
scans); pass --user for the per-user `%LOCALAPPDATA%\\Programs\\Common\\VST3`
location.

x86_64 builds default to `-C target-cpu=x86-64-v3` (AVX2 + FMA + BMI2)
so `wide`'s compile-time SIMD dispatch picks the wider path. aarch64
builds use NEON unconditionally and get no extra flag. Override with
`--target-cpu`.

Options:
  --clap           CLAP only
  --vst3           VST3 only
  --user           Install per-user (default; exception: VST3 on Windows
                   defaults to system - pass --user to override).
  --system         Install system-wide (sudo / admin required).
  --shell          Build dynamic shells + per-plugin logic dylibs.
  --debug          Cargo dev profile (faster compile, slower DSP).
  --no-build       Skip build, install existing artifacts.
  --features <list>
                   Extra Cargo features for the plugin crate, comma/space-
                   separated. Additive, applied to every underlying build.
                   Format features (clap/vst3/...) are reserved.
  --no-default-features
                   Don't re-add the plugin's non-format default features
                   (e.g. ara) to each per-format build.
  -p <crate>       Install only the plugin with this cargo crate name.
  --target-cpu <value>
                   Override the x86_64 default. Accepted values:
                     baseline   no flag (rustc default = x86-64 / SSE2)
                     v2|v3|v4   x86-64-v<N> (v3 is the implicit default)
                     native     -C target-cpu=native (local-dev only;
                                won't run on machines without the
                                build host's exact feature set)
                     <literal>  passed verbatim to rustc (apple-m1, znver4)
  -h, --help       Show this message"
    );
}

/// Resolve the per-format effective scope and print the policy note
/// (once per `cargo moose` invocation) when the user-visible result
/// differs from a plain `--user`: a per-(format, OS) default
/// (Windows VST3).
fn scope_for(format: Format, requested: Option<InstallScope>) -> InstallScope {
    let (effective, note) = effective_scope(format, requested);
    if let Some(msg) = note {
        note_once(msg);
    }
    effective
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub(crate) fn install_clap(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    scope: InstallScope,
    factory_presets: Option<&FactoryPresets>,
) -> Res {
    #[cfg(not(target_os = "macos"))]
    let dylib = release_lib(root, &format!("{}_clap", p.dylib_stem()));
    #[cfg(target_os = "macos")]
    let dylib = crate::release_bundle_bin(root, &p.dylib_stem(), "_clap");
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let clap_dir = scope.clap_dir();
    let bundle = clap_dir.join(format!("{}.clap", p.file_stem()));

    #[cfg(target_os = "macos")]
    {
        // CLAP on macOS uses the loadable-bundle layout that hosts
        // (Bitwig, Studio One) require per Apple's bundle conventions.
        // Earlier moose versions wrote a flat dylib renamed `.clap`;
        // if that's still on disk at `bundle`, clear it before
        // building the directory.
        let contents = bundle.join("Contents");
        let macos_dir = contents.join("MacOS");
        let exec_name = p.file_stem();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{exec_name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
            display_name = p.name,
            bundle_id = p.bundle_id,
            vendor_id = config.vendor.id,
        );
        let plist_tmp = tmp_manifests()
            .join(format!("{}_clap.plist", p.bundle_id))
            .to_string_lossy()
            .to_string();
        fs_ctx::write(&plist_tmp, &plist)?;

        if scope.needs_sudo() {
            if bundle.exists() && !bundle.is_dir() {
                run_sudo("rm", &[OsStr::new("-f"), bundle.as_os_str()])?;
            }
            run_sudo("mkdir", &[OsStr::new("-p"), macos_dir.as_os_str()])?;
            let dst_dylib = macos_dir.join(&exec_name);
            run_sudo("cp", &[dylib.as_os_str(), dst_dylib.as_os_str()])?;
            let dst_plist = contents.join("Info.plist");
            run_sudo("cp", &[OsStr::new(&plist_tmp), dst_plist.as_os_str()])?;
        } else {
            if bundle.exists() && !bundle.is_dir() {
                fs::remove_file(&bundle)?;
            }
            fs_ctx::create_dir_all(&macos_dir)?;
            fs_ctx::copy(&dylib, macos_dir.join(&exec_name))?;
            fs_ctx::copy(&plist_tmp, contents.join("Info.plist"))?;
        }

        // Presets are part of the bundle's sealed Resources - they
        // must land before codesign or the signature won't cover
        // them.
        if let Some(fp) = factory_presets {
            presets::emit_trucepreset_tree(
                fp,
                &contents.join("Resources/Presets"),
                scope.needs_sudo(),
                &format!("{}-clap", p.bundle_id),
            )?;
        }

        codesign_bundle(
            bundle.to_str().unwrap(),
            &crate::application_identity(),
            scope.needs_sudo(),
        )?;
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Linux installs are always per-user; Windows system-scope
        // writes succeed when the cargo-moose process is elevated and
        // bubble an OS-level EACCES otherwise (Windows has no per-
        // command sudo - elevation is per-process via UAC).
        fs_ctx::create_dir_all(&clap_dir)?;
        fs_ctx::copy(&dylib, &bundle)?;
    }

    #[cfg(not(target_os = "macos"))]
    if let Some(fp) = factory_presets {
        // The `.clap` is a single file here, so presets land in a
        // `<stem>.presets/` sibling directory; the wrapper's
        // discovery provider derives the same path from its own
        // dylib location at scan time.
        presets::emit_trucepreset_tree(
            fp,
            &clap_dir.join(format!("{}.presets", p.file_stem())),
            scope.needs_sudo(),
            &format!("{}-clap", p.bundle_id),
        )?;
    }

    crate::log_output(format!("CLAP: {}", bundle.display()));
    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn install_vst3(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    scope: InstallScope,
    factory_presets: Option<&FactoryPresets>,
) -> Res {
    #[cfg(not(target_os = "macos"))]
    let dylib = release_lib(root, &format!("{}_vst3", p.dylib_stem()));
    #[cfg(target_os = "macos")]
    let dylib = crate::release_bundle_bin(root, &p.dylib_stem(), "_vst3");
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = scope.vst3_dir().join(format!("{}.vst3", p.file_stem()));

    #[cfg(target_os = "macos")]
    {
        let contents = bundle.join("Contents");
        let macos_dir = contents.join("MacOS");
        let exec_name = p.file_stem();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{exec_name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
            display_name = p.name,
            bundle_id = p.bundle_id,
            vendor_id = config.vendor.id,
        );
        let plist_tmp = tmp_manifests()
            .join(format!("{}_vst3.plist", p.bundle_id))
            .to_string_lossy()
            .to_string();
        fs_ctx::write(&plist_tmp, &plist)?;

        if scope.needs_sudo() {
            run_sudo("mkdir", &[OsStr::new("-p"), macos_dir.as_os_str()])?;
            let dst_dylib = macos_dir.join(&exec_name);
            run_sudo("cp", &[dylib.as_os_str(), dst_dylib.as_os_str()])?;
            let dst_plist = contents.join("Info.plist");
            run_sudo("cp", &[OsStr::new(&plist_tmp), dst_plist.as_os_str()])?;
        } else {
            fs_ctx::create_dir_all(&macos_dir)?;
            fs_ctx::copy(&dylib, macos_dir.join(&exec_name))?;
            fs_ctx::copy(&plist_tmp, contents.join("Info.plist"))?;
        }

        codesign_bundle(
            bundle.to_str().unwrap(),
            &crate::application_identity(),
            scope.needs_sudo(),
        )?;
        crate::log_output(format!("VST3: {}", bundle.display()));
    }

    #[cfg(target_os = "windows")]
    {
        // VST3 on Windows: <vst3_dir>\{name}.vst3\Contents\x86_64-win\{name}.vst3
        let arch_dir = bundle.join("Contents").join("x86_64-win");
        let dst = arch_dir.join(format!("{}.vst3", p.file_stem()));
        fs_ctx::create_dir_all(&arch_dir)?;
        fs_ctx::copy(&dylib, &dst)?;
        crate::log_output(format!("VST3: {}", bundle.display()));
    }

    #[cfg(target_os = "linux")]
    {
        let arch_dir = bundle.join("Contents").join("x86_64-linux");
        let dst = arch_dir.join(format!("{}.so", p.file_stem()));
        fs_ctx::create_dir_all(&arch_dir)?;
        fs_ctx::copy(&dylib, &dst)?;
        crate::log_output(format!("VST3: {}", bundle.display()));
    }

    // The VST3 spec has no in-bundle preset location - hosts scan
    // the per-OS preset directories, so the files land there.
    if let Some(fp) = factory_presets {
        presets::emit_vst3_presets(fp, p, config, scope)?;
    }

    Ok(())
}
