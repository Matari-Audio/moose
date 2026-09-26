//! `cargo moose doctor` - environment diagnostics: Rust toolchain, code
//! signing tools, installed plugins.

#[cfg(target_os = "windows")]
use crate::commands::package::windows;
use crate::format::Format;
use crate::install_scope::InstallScope;
use crate::read_build_env;
#[cfg(target_os = "macos")]
use crate::rustup_has_target;
use crate::{
    Res, check_cmd, dirs, find_on_path, load_config, project_root, tag_fail, tag_info, tag_ok,
    tag_warn,
};
#[cfg(target_os = "windows")]
use crate::{locate_msvc_cl, which_exe};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// doctor - environment diagnostics
// ---------------------------------------------------------------------------

fn print_help() {
    eprintln!(
        "\
Usage: cargo moose doctor

Run environment diagnostics: Rust toolchain, code-signing identities,
installed-plugin scan. Prints a summary report and
exits 0; investigates rather than fixes. Run when something feels
broken about your machine setup before reaching for a build.

Options:
  -h, --help       Show this message."
    );
}

// Returns `Res` for uniformity with the rest of the `cmd_*` dispatch
// table even though every diagnostic only ever prints - the helpers
// don't surface fallible errors today.
#[allow(clippy::unnecessary_wraps, clippy::too_many_lines)]
pub(crate) fn cmd_doctor(args: &[String]) -> Res {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    if let Some(unknown) = args.iter().find(|a| !a.is_empty()) {
        return Err(format!("unknown flag: {unknown}").into());
    }
    eprintln!("moose doctor");
    eprintln!("─────────────────────────────────────────");
    eprintln!();

    let root = project_root();

    // Toolchain
    eprintln!("  Toolchain");
    check_cmd("rustc", &[OsStr::new("--version")], "rustc");
    check_cmd("cargo", &[OsStr::new("--version")], "cargo");
    if root.join("rust-toolchain.toml").exists() {
        eprintln!("    {} rust-toolchain.toml present", tag_ok());
    }
    match crate::util::sccache_wrapper() {
        Some(p) => eprintln!(
            "    {} sccache active at {} - caches rustc invocations across builds",
            tag_ok(),
            p.to_string_lossy()
        ),
        None if env::var_os("RUSTC_WRAPPER").is_some()
            || env::var_os("RUSTC_WORKSPACE_WRAPPER").is_some() =>
        {
            eprintln!(
                "    {} RUSTC_WRAPPER already set - skipping sccache auto-detect",
                tag_info()
            );
        }
        None => eprintln!(
            "    {} sccache not on PATH - optional. `brew install sccache` (or cargo install) caches rustc \
             invocations at the input-hash level and speeds up env-flipping rebuilds",
            tag_info()
        ),
    }

    // Platform tools
    #[cfg(target_os = "macos")]
    {
        eprintln!();
        eprintln!("  macOS");
        check_cmd("xcode-select", &[OsStr::new("-p")], "Xcode CLI tools");
        check_cmd("codesign", &[OsStr::new("--help")], "codesign");
        // Universal packaging (default for `cargo moose package`) needs both
        // Apple Rust targets. Missing targets are a warning, not an error -
        // `--host-only` still works without them.
        let has_x64 = rustup_has_target("x86_64-apple-darwin");
        let has_arm = rustup_has_target("aarch64-apple-darwin");
        match (has_x64, has_arm) {
            (true, true) => eprintln!(
                "    {} Rust targets: x86_64-apple-darwin + aarch64-apple-darwin - `cargo moose package` will produce universal Mach-O binaries",
                tag_ok()
            ),
            (false, true) => eprintln!(
                "    {} Rust target x86_64-apple-darwin missing - run: rustup target add x86_64-apple-darwin (or pass `--host-only` to skip)",
                tag_warn()
            ),
            (true, false) => eprintln!(
                "    {} Rust target aarch64-apple-darwin missing - run: rustup target add aarch64-apple-darwin (or pass `--host-only` to skip)",
                tag_warn()
            ),
            (false, false) => eprintln!(
                "    {} No Apple Rust targets installed - run: rustup target add x86_64-apple-darwin aarch64-apple-darwin (or pass `--host-only` to skip)",
                tag_warn()
            ),
        }
    }
    #[cfg(target_os = "windows")]
    {
        eprintln!();
        eprintln!("  Packaging (Windows)");
        windows::doctor();
    }

    // Compilers
    eprintln!();
    eprintln!("  Compilers");
    #[cfg(not(target_os = "windows"))]
    {
        check_cmd("cc", &[OsStr::new("--version")], "C compiler");
        check_cmd(
            "c++",
            &[OsStr::new("--version")],
            "C++ compiler (VST3 shim)",
        );
    }
    #[cfg(target_os = "windows")]
    {
        // `cl.exe` is only on PATH inside a Developer Command Prompt, but Rust
        // (cc-rs) and CMake both auto-discover MSVC via vswhere - so the bare
        // PATH check would falsely flag the tool as missing on a perfectly
        // working setup. Try PATH first, then fall back to vswhere.
        if which_exe("cl.exe").is_some() {
            check_cmd("cl", &[OsStr::new("/?")], "MSVC compiler (in current PATH)");
        } else {
            match locate_msvc_cl() {
                Some(p) => eprintln!(
                    "    {} MSVC compiler at {} (not in PATH - Rust/CMake auto-discover it via vswhere)",
                    tag_ok(),
                    p.display()
                ),
                None => eprintln!(
                    "    {} MSVC compiler: not found - install VS \"Desktop development with C++\" workload",
                    tag_fail()
                ),
            }
        }
    }

    // Validation tools
    eprintln!();
    eprintln!("  Validation Tools");
    check_which_with_env("pluginval", Some("PLUGINVAL"));
    check_which_with_env("clap-validator", Some("CLAP_VALIDATOR"));

    // Configuration
    eprintln!();
    eprintln!("  Configuration");
    if moose_build::config_file_in(&root).is_some() {
        match load_config() {
            Ok(c) => eprintln!(
                "    {} moose.toml: {} plugins configured",
                tag_ok(),
                c.plugin.len(),
            ),
            Err(e) => eprintln!("    {} moose.toml parse error: {e}", tag_fail()),
        }
    } else {
        eprintln!("    {} moose.toml not found", tag_fail());
    }

    // Plugin install paths - both scopes side-by-side per format.
    // Helps a user who's confused about why a host is finding the
    // wrong copy of their plugin: when the same name appears under
    // both scopes, the host picks one and shadows the other.
    eprintln!();
    eprintln!("  Plugin install paths");
    show_scope_paths();

    eprintln!();
    eprintln!("─────────────────────────────────────────");
    Ok(())
}

#[derive(Clone, Copy)]
struct PathFormat {
    format: Format,
    /// File extension or bundle suffix. Used to count plug-ins in
    /// the directory and to detect cross-scope collisions in
    /// `cargo moose validate`.
    ext: &'static str,
}

const PATH_FORMATS_MACOS: &[PathFormat] = &[
    PathFormat {
        format: Format::Clap,
        ext: "clap",
    },
    PathFormat {
        format: Format::Vst3,
        ext: "vst3",
    },
];

const PATH_FORMATS_WINDOWS: &[PathFormat] = &[
    PathFormat {
        format: Format::Clap,
        ext: "clap",
    },
    PathFormat {
        format: Format::Vst3,
        ext: "vst3",
    },
];

const PATH_FORMATS_LINUX: &[PathFormat] = &[
    PathFormat {
        format: Format::Clap,
        ext: "clap",
    },
    PathFormat {
        format: Format::Vst3,
        ext: "vst3",
    },
];

fn show_scope_paths() {
    let formats: &[PathFormat] = if cfg!(target_os = "macos") {
        PATH_FORMATS_MACOS
    } else if cfg!(target_os = "windows") {
        PATH_FORMATS_WINDOWS
    } else {
        PATH_FORMATS_LINUX
    };

    for f in formats {
        let user_path = f.format.dir(InstallScope::User);
        let system_path = f.format.dir(InstallScope::System);
        report_scope_line(f, "user", InstallScope::User, &user_path);
        // Linux's user and system dirs resolve to the same path -
        // skip the duplicate row to keep the matrix readable.
        if user_path != system_path {
            report_scope_line(f, "system", InstallScope::System, &system_path);
        }
    }
}

fn report_scope_line(f: &PathFormat, scope_label: &str, scope: InstallScope, path: &Path) {
    let label = format!("{} {}:", f.format.label(), scope_label);
    report_path_line(&label, scope.needs_sudo(), path, f.ext);
}

fn report_path_line(label: &str, needs_sudo: bool, path: &Path, ext: &str) {
    if !path.exists() {
        eprintln!(
            "    {label:<14} {}{}- not present",
            display_path(path),
            spacer(path)
        );
        return;
    }
    let count = count_bundles_with_ext(path, ext);
    let plural = if count == 1 { "" } else { "s" };
    let count_str = format!(" ({count} plug-in{plural})");
    let state = if needs_sudo {
        format!("{} needs sudo{count_str}", tag_warn())
    } else if path_is_writable(path) {
        format!("{} writable{count_str}", tag_ok())
    } else {
        format!("{} not writable{count_str}", tag_warn())
    };
    eprintln!(
        "    {label:<14} {}{}{state}",
        display_path(path),
        spacer(path)
    );
}

/// Tab-aligned spacer between the path and the state column.
/// Pads the path column to ~46 chars so the state markers line up
/// regardless of how long the resolved path is.
fn spacer(path: &Path) -> String {
    let width = display_path(path).chars().count();
    let target = 46usize;
    if width < target {
        " ".repeat(target - width)
    } else {
        "  ".to_string()
    }
}

/// Render `path` as `~/...` when it's under `$HOME`, otherwise as
/// the absolute path. Keeps the matrix readable on macOS / Linux
/// where the user-scope paths all begin with the home directory.
fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rel) = path.strip_prefix(&home)
    {
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}

fn count_bundles_with_ext(dir: &Path, ext: &str) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let want = format!(".{ext}");
    entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .to_lowercase()
                .ends_with(&want.to_lowercase())
        })
        .count()
}

/// Probe writability by trying to create a tempfile in `dir`. Avoids
/// false positives from `metadata().permissions().readonly()` which
/// only reports the file mode, not the effective access - system
/// dirs on macOS are 0755 and would read as "writable" without
/// surfacing the sudo requirement.
fn path_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".moose-doctor-write-probe-{}", std::process::id()));
    match fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Like `check_which`, but consults `env_var` (process env, then
/// `.cargo/config.toml` `[env]`) before falling back to `$PATH`. Lets users
/// point doctor at tools installed outside `$PATH` - useful for `.app`-bundled
/// binaries (pluginval) or sibling source checkouts (clap-validator).
fn check_which_with_env(name: &str, env_var: Option<&str>) {
    if let Some(var) = env_var
        && let Some(path) = read_build_env(var)
    {
        let p = PathBuf::from(&path);
        if p.is_file() {
            eprintln!("    {} {name}: {path} (via ${var})", tag_ok());
            return;
        }
        eprintln!(
            "    {} {name}: ${var}={path} but file not found - falling back to $PATH",
            tag_warn()
        );
    }
    // PATH lookup is done in-process: Windows has no `which` binary
    // (and `where.exe` isn't on every minimal install), so doing the
    // walk ourselves keeps doctor's behavior identical across
    // platforms. `find_on_path` appends `.exe` on Windows so callers
    // pass a bare tool name.
    if let Some(path) = find_on_path(name) {
        eprintln!("    {} {name}: {}", tag_ok(), path.display());
    } else {
        let hint = env_var
            .map(|v| format!(" (or set ${v} in shell or .cargo/config.toml [env])"))
            .unwrap_or_default();
        eprintln!("    {} {name}: not found{hint}", tag_warn());
    }
}
