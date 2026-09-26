//! `cargo moose uninstall` - remove plugin bundles for the current project,
//! or with `--stale` evict vendor-matching bundles no longer in `moose.toml`.

use crate::install_scope::{InstallScope, set_cli_install_scope};
use crate::{PluginDef, Res, confirm_prompt, load_config};
// `run_sudo` is macOS-only (Linux is always per-user, Windows uses
// per-process UAC elevation rather than per-command sudo).
#[cfg(target_os = "macos")]
use crate::run_sudo;
use moose_utils::shell_sidecar::sidecar_path;
#[cfg(target_os = "macos")]
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

struct RemoveTarget {
    format: &'static str,
    path: PathBuf,
    needs_sudo: bool,
}

#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_uninstall(args: &[String]) -> Res {
    let config = load_config()?;

    let mut clap = false;
    let mut vst3 = false;
    let mut standalone = false;
    let mut dry_run = false;
    let mut yes = false;
    let mut stale = false;
    let mut crate_filter: Option<String> = None;
    let mut name_filter: Option<String> = None;
    let mut cli_scope: Option<InstallScope> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--clap" => clap = true,
            "--vst3" => vst3 = true,
            "--standalone" => standalone = true,
            "--dry-run" => dry_run = true,
            "--yes" | "-y" => yes = true,
            "--stale" => stale = true,
            "--user" => set_cli_install_scope(&mut cli_scope, InstallScope::User)?,
            "--system" => set_cli_install_scope(&mut cli_scope, InstallScope::System)?,
            "--ask" => {
                return Err(
                    "--ask is not valid for `cargo moose uninstall` (no end user to prompt). \
                     Use --user or --system."
                        .into(),
                );
            }
            "-p" => {
                crate_filter = Some(crate::util::arg_value(args, &mut i, "-p")?.to_string());
            }
            "-n" => {
                name_filter = Some(crate::util::arg_value(args, &mut i, "-n")?.to_string());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
        i += 1;
    }

    // Without an explicit scope flag, scan both user and system -
    // a dev who switched scopes mid-iteration may have stale copies
    // in the other half of the disk.
    let scopes_to_scan: Vec<InstallScope> = match cli_scope {
        Some(InstallScope::User) => vec![InstallScope::User],
        Some(InstallScope::System) => vec![InstallScope::System],
        None => vec![InstallScope::User, InstallScope::System],
    };
    // Captured before the default-fill below so the post-loop sidecar
    // cleanup can tell "user passed no format flag → uninstall
    // everything for these plugins" apart from "user picked specific
    // formats → leave shell sidecars alone for the others".
    let all_formats_default = !clap && !vst3 && !standalone;

    // Default: all formats if none specified.
    if all_formats_default {
        clap = true;
        vst3 = true;
        standalone = true;
    }

    let vendor = &config.vendor.name;
    let known_names: Vec<&str> = config.plugin.iter().map(|p| p.name.as_str()).collect();

    let mut targets: Vec<RemoveTarget> = Vec::new();
    // Collected in the non-stale branch below; used after the
    // bundle-removal loop to clean up `~/.moose/shell/<crate>.path`
    // sidecars when the user is uninstalling all formats for a
    // plugin. Empty for `--stale` (we only have display names there,
    // not crate names - sidecars stay).
    let mut crate_names_for_sidecar_cleanup: Vec<String> = Vec::new();

    if stale {
        // --stale: find vendor-matching bundles NOT in the current project
        let scan = |dir: &Path,
                    ext: &str,
                    format: &'static str,
                    match_token: &str,
                    known_stems: &[&str],
                    needs_sudo: bool,
                    targets: &mut Vec<RemoveTarget>| {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if !name.contains(match_token) {
                        continue;
                    }
                    // Strip extension to get the stem
                    let stem = name.trim_end_matches(&format!(".{ext}"));
                    if known_stems.contains(&stem) {
                        continue;
                    }
                    targets.push(RemoveTarget {
                        format,
                        path: entry.path(),
                        needs_sudo,
                    });
                }
            }
        };

        if clap {
            for s in &scopes_to_scan {
                scan(
                    &s.clap_dir(),
                    "clap",
                    "CLAP",
                    vendor,
                    &known_names,
                    s.needs_sudo(),
                    &mut targets,
                );
            }
        }
        if vst3 {
            for s in &scopes_to_scan {
                scan(
                    &s.vst3_dir(),
                    "vst3",
                    "VST3",
                    vendor,
                    &known_names,
                    s.needs_sudo(),
                    &mut targets,
                );
            }
        }
        // `--stale --standalone` cleans up legacy `<Name>.standalone.app`
        // bundles only - the historical convention `cargo moose package`
        // used before the rename. The current `<Plugin>.app` layout
        // collides with arbitrary unrelated apps the user installed
        // from anywhere; vendor-string substring matching isn't enough
        // to confidently delete a `.app` from `/Applications`, so we
        // skip those here. Run with `--stale` + `-p` / `-n` for a
        // targeted sweep instead.
        #[cfg(target_os = "macos")]
        if standalone {
            let scan_legacy_standalone =
                |dir: &Path, needs_sudo: bool, targets: &mut Vec<RemoveTarget>| {
                    if let Ok(entries) = fs::read_dir(dir) {
                        for entry in entries.flatten() {
                            let name = entry.file_name();
                            let name_str = name.to_string_lossy();
                            if !name_str.ends_with(".standalone.app") {
                                continue;
                            }
                            if !name_str.contains(vendor) {
                                continue;
                            }
                            let display = name_str.trim_end_matches(".standalone.app");
                            if known_names.contains(&display) {
                                continue;
                            }
                            targets.push(RemoveTarget {
                                format: "Standalone",
                                path: entry.path(),
                                needs_sudo,
                            });
                        }
                    }
                };
            for s in &scopes_to_scan {
                scan_legacy_standalone(&s.standalone_dir(), s.needs_sudo(), &mut targets);
            }
        }

        // Apply -p (substring match on filename) or -n (exact display name match)
        if let Some(ref filter) = crate_filter {
            let filter_lower = filter.to_lowercase();
            targets.retain(|t| {
                t.path
                    .file_name()
                    .is_some_and(|f| f.to_string_lossy().to_lowercase().contains(&filter_lower))
            });
        } else if let Some(ref filter) = name_filter {
            let filter_lower = filter.to_lowercase();
            targets.retain(|t| {
                let fname = t
                    .path
                    .file_stem()
                    .map(|f| f.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                fname == filter_lower
            });
        }
    } else {
        // Normal mode: remove bundles for plugins in the project

        // Filter plugins by crate name (-p) or display name (-n)
        let plugins: Vec<&PluginDef> = if let Some(ref filter) = crate_filter {
            let matched: Vec<_> = config
                .plugin
                .iter()
                .filter(|p| p.crate_name == *filter)
                .collect();
            if matched.is_empty() {
                return Err(format!(
                    "No plugin with crate name '{filter}'. Available: {}",
                    config
                        .plugin
                        .iter()
                        .map(|p| format!("{} (-p {})", p.name, p.crate_name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            matched
        } else if let Some(ref filter) = name_filter {
            let filter_lower = filter.to_lowercase();
            let matched: Vec<_> = config
                .plugin
                .iter()
                .filter(|p| p.name.to_lowercase() == filter_lower)
                .collect();
            if matched.is_empty() {
                return Err(format!(
                    "No plugin with name '{filter}'. Available: {}",
                    config
                        .plugin
                        .iter()
                        .map(|p| format!("\"{}\" (-p {})", p.name, p.crate_name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            matched
        } else {
            config.plugin.iter().collect()
        };

        if all_formats_default {
            crate_names_for_sidecar_cleanup.extend(plugins.iter().map(|p| p.crate_name.clone()));
        }

        let push_if_exists =
            |format: &'static str, path: PathBuf, needs_sudo: bool, targets: &mut Vec<_>| {
                if path.exists() && !targets.iter().any(|t: &RemoveTarget| t.path == path) {
                    targets.push(RemoveTarget {
                        format,
                        path,
                        needs_sudo,
                    });
                }
            };
        for p in &plugins {
            if clap {
                for s in &scopes_to_scan {
                    let path = s.clap_dir().join(format!("{}.clap", p.file_stem()));
                    push_if_exists("CLAP", path, s.needs_sudo(), &mut targets);
                }
            }
            if vst3 {
                for s in &scopes_to_scan {
                    let path = s.vst3_dir().join(format!("{}.vst3", p.file_stem()));
                    push_if_exists("VST3", path, s.needs_sudo(), &mut targets);
                }
            }
            if standalone {
                #[cfg(target_os = "macos")]
                {
                    for s in &scopes_to_scan {
                        // Current convention: plain `<Plugin>.app` so
                        // Spotlight / Launch Services index it as a
                        // regular application. The historical
                        // `<Plugin>.standalone.app` name is checked too
                        // for users upgrading from older installers.
                        let dir = s.standalone_dir();
                        push_if_exists(
                            "Standalone",
                            dir.join(format!("{}.app", p.file_stem())),
                            s.needs_sudo(),
                            &mut targets,
                        );
                        push_if_exists(
                            "Standalone",
                            dir.join(format!("{}.standalone.app", p.file_stem())),
                            s.needs_sudo(),
                            &mut targets,
                        );
                    }
                }
                // Linux / Windows standalone paths are handled by the
                // platform installer (a bare ELF under `~/.local/bin`
                // on Linux, `%PROGRAMFILES%\<Vendor>\<Plugin>\...exe`
                // on Windows). Uninstall there is the OS package
                // manager's responsibility - `cargo moose uninstall`
                // never put the file there in the first place.
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = p;
                }
            }
        }
    }

    if targets.is_empty() {
        eprintln!("No installed plugins found to remove.");
        return Ok(());
    }

    // Show summary
    eprintln!("The following plugins will be removed:\n");
    for t in &targets {
        eprintln!("  {:<5} {}", t.format, t.path.display());
    }
    eprintln!();

    if dry_run {
        eprintln!("Dry run - nothing was removed.");
        return Ok(());
    }

    if !yes && !confirm_prompt(&format!("Remove {} bundle(s)?", targets.len())) {
        eprintln!("Cancelled.");
        return Ok(());
    }

    // Remove bundles
    let mut errors = 0u32;

    for t in &targets {
        // `needs_sudo` only drives the macOS path: on Linux every
        // scope is per-user, and on Windows the cargo-moose process
        // is either elevated (direct fs ops succeed) or it isn't
        // (the OS returns EACCES from `remove_*` and that surfaces
        // unchanged).
        let result: Res = {
            #[cfg(target_os = "macos")]
            {
                if t.needs_sudo {
                    run_sudo("rm", &[OsStr::new("-rf"), t.path.as_os_str()])
                } else {
                    fs::remove_dir_all(&t.path)
                        .or_else(|_| fs::remove_file(&t.path))
                        .map_err(Into::into)
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = t.needs_sudo;
                fs::remove_dir_all(&t.path)
                    .or_else(|_| fs::remove_file(&t.path))
                    .map_err(Into::into)
            }
        };

        let name = t.path.file_name().unwrap_or_default().to_string_lossy();
        match result {
            Ok(()) => eprintln!("  \u{2713} {:<5} {}", t.format, name),
            Err(e) => {
                eprintln!("  \u{2717} {:<5} {} ({})", t.format, name, e);
                errors += 1;
            }
        }
    }

    // Clean up `~/.moose/shell/<crate>.path` sidecars for plugins
    // whose entire format set is being uninstalled. Skipped on
    // partial uninstalls (`--clap`, `-p` etc. without all formats)
    // since the sidecar is shared across format wrappers and
    // removing it would break shell-mode for any still-installed
    // formats. Skipped on `--stale` because we don't have crate
    // names there.
    for crate_name in &crate_names_for_sidecar_cleanup {
        if let Some(path) = sidecar_path(crate_name)
            && path.exists()
        {
            match fs::remove_file(&path) {
                Ok(()) => eprintln!("  \u{2713} sidecar {}", path.display()),
                Err(e) => eprintln!("  \u{2717} sidecar {} ({})", path.display(), e),
            }
        }
    }

    if errors > 0 {
        eprintln!("\n{errors} error(s). Check permissions or run with sudo.");
    } else {
        eprintln!("\nDone. Restart your DAW to rescan.");
    }
    Ok(())
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo moose uninstall [--clap] [--vst3] [--standalone] [--user|--system] [-p <crate>] [-n <name>]
                             [--stale] [--dry-run] [--yes]

Uninstall plugin bundles for this project. Default: all formats,
all plugins, both user + system scopes. Asks for confirmation.

Options:
  --clap           CLAP only
  --vst3           VST3 only
  --standalone     Standalone host app only (.app, macOS only)
  --user           Only uninstall from per-user directories.
  --system         Only uninstall from system directories.
  -p <crate>       Filter by cargo crate name.
  -n <name>        Filter by display name.
  --stale          Uninstall vendor bundles NOT in the current project.
  --dry-run        Show what would be uninstalled without deleting.
  --yes, -y        Skip confirmation prompt.
  -h, --help       Show this message"
    );
}
