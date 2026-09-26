//! `cargo truce validate` - drive pluginval (VST3) and
//! clap-validator (CLAP) against the project's installed bundles, with
//! shadow-install collision detection.

use crate::format::Format;
use crate::install_scope::InstallScope;
#[cfg(target_os = "macos")]
use crate::tmp_verify;
use crate::{PluginDef, Res, dirs, load_config, tag_warn};
use std::ffi::OsStr;
#[cfg(target_os = "macos")]
use std::fs;
use std::path::Path;

/// A `Command` for a validator that loads the plugin bundle, with
/// cargo-injected dynamic-linker vars scrubbed. `cargo run -- validate`
/// injects `DYLD_FALLBACK_LIBRARY_PATH` (macOS) / `LD_LIBRARY_PATH`
/// (Linux) pointing at target/debug deps; inherited by a child that
/// `dlopen`s the bundle, they break its dylib resolution (pluginval
/// scanned zero types; clap-validator loads the same way). The wider `DYLD_*` family is scrubbed for the same
/// reason - any of them can redirect the bundle's resolution.
fn validator_command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    for var in [
        "DYLD_FALLBACK_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "DYLD_FALLBACK_FRAMEWORK_PATH",
        "DYLD_INSERT_LIBRARIES",
        "LD_LIBRARY_PATH",
        "LD_PRELOAD",
    ] {
        cmd.env_remove(var);
    }
    cmd
}
use std::process::Command;

/// Print a one-line warning when the same plugin is installed under
/// both user and system scope. Both copies are valid bundles; the
/// host picks one at scan time and shadows the other, which is a
/// frequent cause of "DAW loads my old build" support questions.
fn warn_on_scope_collision(format: Format, user_path: &Path, system_path: &Path) {
    // On platforms with no distinct system-scope plug-in dir (Linux,
    // Windows for some formats), `InstallScope::User` and `::System`
    // resolve to the same path - a single install can't shadow itself.
    if user_path == system_path {
        return;
    }
    if user_path.exists() && system_path.exists() {
        eprintln!(
            "    {} {} installed in both scopes:",
            tag_warn(),
            format.label(),
        );
        eprintln!("        • user:   {}", user_path.display());
        eprintln!("        • system: {}", system_path.display());
        eprintln!(
            "        Hosts pick one at scan time; remove the stale copy with \
             `cargo truce uninstall --user` or `--system`."
        );
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn cmd_validate(args: &[String]) -> Res {
    let config = load_config()?;

    let mut run_pluginval = false;
    let mut run_clap = false;
    // Track explicit per-format flags so a missing validator counts as a
    // failure for CI (`--clap`, `--pluginval`, …) but stays a warning for
    // a casual `cargo truce validate` run on a host that's missing some
    // tools. `--all` keeps the casual semantics.
    let mut pluginval_explicit = false;
    let mut clap_explicit = false;
    let mut plugin_filter: Option<String> = None;
    // Forwarded to pluginval. Lets CI skip the editor-instantiation
    // probe on hosts where the GL stack can't satisfy a real plugin
    // editor's `glXChooseFBConfig` call (headless Linux runners with
    // no GPU + software-only Xorg/Xvfb don't advertise FBConfigs).
    let mut skip_gui_tests = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pluginval" => {
                run_pluginval = true;
                pluginval_explicit = true;
            }
            "--clap" => {
                run_clap = true;
                clap_explicit = true;
            }
            "--all" => {
                run_pluginval = true;
                run_clap = true;
            }
            "--skip-gui-tests" => {
                skip_gui_tests = true;
            }
            "-p" => {
                plugin_filter = Some(crate::util::arg_value(args, &mut i, "-p")?.to_string());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
        i += 1;
    }
    if !run_pluginval && !run_clap {
        run_pluginval = true;
        run_clap = true;
    }

    let plugins: Vec<&PluginDef> = super::pick_plugins(&config, plugin_filter.as_deref())?;

    let mut failures = 0;

    // pluginval (VST3)
    if run_pluginval {
        eprintln!("\npluginval (VST3)\n");
        let pluginval = find_pluginval();
        if let Some(pv) = pluginval {
            for p in &plugins {
                let user_path = InstallScope::User
                    .vst3_dir()
                    .join(format!("{}.vst3", p.file_stem()));
                let system_path = InstallScope::System
                    .vst3_dir()
                    .join(format!("{}.vst3", p.file_stem()));
                // Validate the system bundle when it's there (the
                // historical default), else fall through to user.
                let validate_path = if system_path.exists() {
                    system_path.clone()
                } else if user_path.exists() {
                    user_path.clone()
                } else {
                    eprintln!("  {} ... SKIP (not installed)", p.name);
                    continue;
                };
                eprint!("  {} ... ", p.name);
                let mut cmd = validator_command(&pv);
                cmd.args([
                    "--validate",
                    validate_path.to_str().unwrap(),
                    "--strictness-level",
                    "10",
                ]);
                if skip_gui_tests {
                    cmd.arg("--skip-gui-tests");
                }
                let output = cmd.output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stdout.contains("SUCCESS") || output.status.success() {
                    eprintln!("PASS");
                } else {
                    eprintln!("FAIL");
                    if !stdout.is_empty() {
                        eprintln!("{stdout}");
                    }
                    if !stderr.is_empty() {
                        eprintln!("{stderr}");
                    }
                    failures += 1;
                }
                warn_on_scope_collision(Format::Vst3, &user_path, &system_path);
            }
        } else {
            eprintln!("  pluginval not found. Install from https://github.com/Tracktion/pluginval");
            if pluginval_explicit {
                failures += 1;
            }
        }
    }

    // clap-validator (CLAP)
    if run_clap {
        eprintln!("\nclap-validator (CLAP)\n");
        let clap_validator = find_clap_validator();
        if let Some(cv) = clap_validator {
            // Project-local scratch for the macOS bundle-wrap fallback.
            // `cargo clean` sweeps it, and it stays off the system
            // `/tmp` so nothing outside the repo gets touched. On
            // Linux/Windows we hand clap-validator the installed file
            // directly, so the scratch dir is never created there.
            #[cfg(target_os = "macos")]
            let scratch = {
                let s = tmp_verify().join("clap-validate");
                let _ = fs::create_dir_all(&s);
                s
            };

            for p in &plugins {
                let clap_name = format!("{}.clap", p.file_stem());
                let user_path = InstallScope::User.clap_dir().join(&clap_name);
                let system_path = InstallScope::System.clap_dir().join(&clap_name);
                // Prefer the user-scope bundle (the default install
                // location); fall through to system-scope if the
                // user installed there instead.
                let installed = if user_path.exists() {
                    user_path.clone()
                } else {
                    system_path.clone()
                };

                if !installed.exists() {
                    eprintln!("  {} ... SKIP (not installed)", p.name);
                    continue;
                }

                // CLAP plugin shape is per-platform:
                //   macOS: a `.clap` *bundle* directory with a binary
                //          at `Contents/MacOS/<name>`. The scratch-
                //          bundle branch below is a fallback for
                //          flat-file `.clap` installs that some
                //          third-party tools still produce; truce's
                //          own installer writes the bundle layout.
                //   Linux:   a `.so` renamed `.clap`. dlopen-loadable
                //          directly - no bundle.
                //   Windows: a `.dll` renamed `.clap`. LoadLibrary-
                //          loadable directly - no bundle.
                #[cfg(target_os = "macos")]
                let validate_path = if installed.join("Contents/MacOS").is_dir() {
                    installed.clone()
                } else {
                    let bundle = scratch.join(&clap_name);
                    let macos = bundle.join("Contents/MacOS");
                    let _ = fs::create_dir_all(&macos);
                    let bin_name = clap_name.trim_end_matches(".clap");
                    let _ = fs::copy(&installed, macos.join(bin_name));
                    bundle
                };
                #[cfg(not(target_os = "macos"))]
                let validate_path = installed.clone();

                eprint!("  {} ... ", p.name);
                let mut cmd = validator_command(&cv);
                cmd.args(["validate", &validate_path.to_string_lossy()]);
                // clap-validator requires location paths to start
                // with '/', but the CLAP header defines FILE
                // locations as OS paths ('\' separators work on
                // Windows) - so spec-compliant Windows paths can
                // never pass its preset-discovery tests (Surge XT
                // fails them identically). Skip those tests here
                // until the validator accepts Windows paths.
                #[cfg(target_os = "windows")]
                cmd.args(["--test-filter", "preset-discovery", "--invert-filter"]);
                let output = cmd.output()?;

                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let combined = format!("{stdout}{stderr}");

                if output.status.success() && !combined.contains("FAILED") {
                    eprintln!("PASS{}", parse_clap_summary(&combined));
                } else {
                    eprintln!("FAIL");
                    if !stdout.is_empty() {
                        eprintln!("{stdout}");
                    }
                    if !stderr.is_empty() {
                        eprintln!("{stderr}");
                    }
                    failures += 1;
                }
                warn_on_scope_collision(Format::Clap, &user_path, &system_path);
            }

            #[cfg(target_os = "macos")]
            let _ = fs::remove_dir_all(&scratch);
        } else {
            eprintln!("  clap-validator not found.");
            eprintln!(
                "  Install: cargo install --git https://github.com/free-audio/clap-validator"
            );
            eprintln!("  Or set CLAP_VALIDATOR=/path/to/clap-validator");
            if clap_explicit {
                failures += 1;
            }
        }
    }

    eprintln!();
    if failures > 0 {
        Err(format!("{failures} validation(s) failed").into())
    } else {
        eprintln!("All validations passed.");
        Ok(())
    }
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce validate [--pluginval] [--clap] [--all]
                            [--skip-gui-tests] [-p <crate>]

Run validation tools on installed plugins. With no flag, runs every
available validator.

Options:
  --pluginval      VST3 validation via pluginval.
  --clap           CLAP validation via clap-validator.
  --all            Run every available validator (default).
  --skip-gui-tests Forwarded to pluginval as `--skip-gui-tests`. Use
                   on headless Linux CI without a GPU: the editor
                   probe needs FBConfigs the software-only GL stack
                   doesn't advertise.
  -p <crate>       Validate only the plugin with this cargo crate name.
  -h, --help       Show this message"
    );
}

/// Pull the test counts out of clap-validator's summary line, e.g.
/// `"20 tests run, 16 passed, 0 failed, 4 skipped, 1 warnings"`. Returns
/// `" (16/20, 4 skipped)"` or an empty string if the summary isn't found.
fn parse_clap_summary(output: &str) -> String {
    let Some(summary) = output.lines().find(|l| l.contains("tests run")) else {
        return String::new();
    };
    let pick = |key: &str| -> Option<u32> {
        let idx = summary.find(key)?;
        summary[..idx]
            .split(|c: char| !c.is_ascii_digit())
            .rfind(|s| !s.is_empty())?
            .parse()
            .ok()
    };
    match (pick("tests run"), pick("passed"), pick("skipped")) {
        (Some(total), Some(passed), Some(skipped)) if skipped > 0 => {
            format!(" ({passed}/{total}, {skipped} skipped)")
        }
        (Some(total), Some(passed), _) => format!(" ({passed}/{total})"),
        _ => String::new(),
    }
}

fn find_pluginval() -> Option<String> {
    // Env-var override takes precedence - CI uses it to point at a
    // cached download outside the standard locations.
    if let Ok(path) = std::env::var("PLUGINVAL")
        && Path::new(&path).exists()
    {
        return Some(path);
    }
    // Common locations.
    let candidates = [
        "/Applications/pluginval.app/Contents/MacOS/pluginval",
        "/usr/local/bin/pluginval",
    ];
    for c in candidates {
        if Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    // PATH lookup.
    if Command::new("pluginval").arg("--help").output().is_ok() {
        return Some("pluginval".to_string());
    }
    None
}

fn find_clap_validator() -> Option<String> {
    // Check env var override
    if let Ok(path) = std::env::var("CLAP_VALIDATOR")
        && Path::new(&path).exists()
    {
        return Some(path);
    }
    // Check PATH
    if Command::new("clap-validator")
        .arg("--version")
        .output()
        .is_ok()
    {
        return Some("clap-validator".to_string());
    }
    // Check cargo install location
    if let Some(home) = dirs::home_dir() {
        let cargo_bin = home.join(".cargo/bin/clap-validator");
        if cargo_bin.exists() {
            return Some(cargo_bin.to_string_lossy().into());
        }
    }
    None
}
