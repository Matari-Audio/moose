//! `cargo moose status` - scan installed plugin bundles for this
//! workspace's plugins.
//!
//! macOS-only: every path it scans (`/Library/Audio/Plug-Ins/...`,
//! `~/Library/Audio/Plug-Ins/...`) is Apple-specific. Linux / Windows
//! are handled with a clean "not supported" message instead of an
//! empty banner that suggests nothing was found.

use crate::Res;

fn print_help() {
    eprintln!(
        "\
Usage: cargo moose status

Scan installed plugin bundles for this workspace's plugins (matched
exactly by the on-disk name the installer writes, per format).
macOS-only - every path scanned (/Library/Audio/Plug-Ins/...,
~/Library/Audio/Plug-Ins/...) is Apple-specific.

Options:
  -h, --help       Show this message."
    );
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn cmd_status(args: &[String]) -> Res {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    Err(
        "`cargo moose status` is macOS-only - every directory it scans \
         (`/Library/Audio/Plug-Ins/...`) is Apple-specific. \
         For Linux / Windows, list bundles directly under your DAW's \
         configured plug-in path."
            .into(),
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn cmd_status(args: &[String]) -> Res {
    use crate::{dirs, load_config};
    use std::collections::HashSet;
    use std::path::PathBuf;

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    if let Some(unknown) = args.iter().find(|a| !a.is_empty()) {
        return Err(format!("unknown flag: {unknown}").into());
    }

    let config = load_config()?;
    let home = dirs::require_home_dir()?;

    // Per-format expected bundle names, derived from `moose.toml`.
    // Matching is exact (the installer writes the same names), so an
    // unrelated plugin from another workspace with the same vendor
    // can't surface here.
    let expect_with_ext = |ext: &str| -> HashSet<String> {
        config
            .plugin
            .iter()
            .map(|p| format!("{}.{ext}", p.file_stem()))
            .collect()
    };
    let clap_names = expect_with_ext("clap");
    let vst3_names = expect_with_ext("vst3");

    // Each format can land in either user or system scope; both are
    // scanned so a per-user install isn't invisible to status.
    let sections: &[(&str, [PathBuf; 2], &HashSet<String>)] = &[
        (
            "CLAP",
            [
                home.join("Library/Audio/Plug-Ins/CLAP"),
                PathBuf::from("/Library/Audio/Plug-Ins/CLAP"),
            ],
            &clap_names,
        ),
        (
            "VST3",
            [
                home.join("Library/Audio/Plug-Ins/VST3"),
                PathBuf::from("/Library/Audio/Plug-Ins/VST3"),
            ],
            &vst3_names,
        ),
    ];

    for (i, (label, paths, expected)) in sections.iter().enumerate() {
        if i > 0 {
            eprintln!();
        }
        eprintln!("{label}");
        for path in paths {
            scan_expected_entries(path, expected)?;
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn scan_expected_entries(
    dir: &std::path::Path,
    expected: &std::collections::HashSet<String>,
) -> Res {
    use std::fs;
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if expected.contains(name.as_ref()) {
            eprintln!("  {name}");
        }
    }
    Ok(())
}
