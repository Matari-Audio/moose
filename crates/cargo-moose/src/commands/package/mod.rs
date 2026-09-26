//! `cargo moose package` - build, sign, and create installers.
//!
//! Top-level dispatch + the format-flag parsing shared between the macOS
//! `.pkg` pipeline (`macos.rs`), the Linux tarball pipeline (`linux.rs`),
//! and the Windows Inno Setup pipeline (`windows.rs`).

use crate::CargoMooseError;
#[cfg(target_os = "macos")]
use crate::PluginDef;
use crate::Res;

pub(crate) mod stage;
pub(crate) mod verify;

#[cfg(target_os = "macos")]
pub(crate) mod macos;

// Linux module always compiles so cross-platform CI catches compile
// errors on macOS / Windows runs. The dispatcher in `cmd_package`
// gates which `cmd_package_*` is invoked at runtime.
#[allow(dead_code)]
pub(crate) mod linux;

// Windows packager + its DPI/icon manifest helper. Gated on
// `windows` because both pull in `windows_sys` APIs that don't exist
// on other targets (unlike the pure-std `linux` module).
#[cfg(target_os = "windows")]
pub(crate) mod windows;
#[cfg(target_os = "windows")]
pub(crate) mod windows_manifest;

/// Composable selection flags shared across platforms. Parsed once at
/// top-level and passed down to the per-platform packagers so each
/// dispatch path applies them uniformly.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
#[derive(Clone, Default, Debug)]
pub(crate) struct SuiteSelection {
    /// `--suite <name>` (repeatable). Empty = every declared suite.
    pub(crate) only_suites: Vec<String>,
    /// `--no-suite`: skip all suite installers.
    pub(crate) no_suite: bool,
    /// `--no-per-plugin`: skip per-plugin installers.
    pub(crate) no_per_plugin: bool,
}

impl SuiteSelection {
    pub(crate) fn want_per_plugin(&self) -> bool {
        !self.no_per_plugin
    }
    pub(crate) fn want_suite(&self, name: &str, bundle_id: &str) -> bool {
        if self.no_suite {
            return false;
        }
        if self.only_suites.is_empty() {
            return true;
        }
        // Accept either the display name or the bundle_id slug - the
        // slug is what users naturally type (and what filenames use).
        self.only_suites.iter().any(|s| s == name || s == bundle_id)
    }

    /// Error if a `--suite <x>` matches no declared suite by name or
    /// `bundle_id`, so a typo / wrong identifier fails loudly instead of
    /// silently producing no suite installer.
    pub(crate) fn validate_against(
        &self,
        declared: &[crate::config::SuiteDef],
    ) -> Result<(), CargoMooseError> {
        let unknown: Vec<&str> = self
            .only_suites
            .iter()
            .filter(|req| {
                !declared
                    .iter()
                    .any(|s| &s.name == *req || &s.bundle_id == *req)
            })
            .map(String::as_str)
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        let available = if declared.is_empty() {
            "(none declared in moose.toml)".to_string()
        } else {
            declared
                .iter()
                .map(|s| format!("{} (\"{}\")", s.bundle_id, s.name))
                .collect::<Vec<_>>()
                .join(", ")
        };
        Err(format!("--suite: no suite matches {unknown:?}. Available suites: {available}").into())
    }
}

/// Strip the suite-selection flags out of `args` before the
/// per-platform parser sees them. Returns the parsed selection plus a
/// new `args` vector with those flags removed.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
/// Pull `--features <list>` occurrences out of the package args and
/// return `(features, remaining_args)`. Kept separate from the per-OS
/// arg parsers so they never see the flag; the features flow to the
/// builds via the invocation global instead.
/// Pull the invocation-global feature flags out of the raw args: the
/// `--features` list and whether `--no-default-features` was passed.
/// Returns them plus the remaining args for the per-OS parsers.
fn extract_features_arg(
    args: &[String],
) -> Result<(Vec<String>, bool, Vec<String>), CargoMooseError> {
    let mut features = Vec::new();
    let mut no_default_features = false;
    let mut rest = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--features" {
            let value = crate::util::arg_value(args, &mut i, "--features")?;
            features.extend(crate::parse_extra_features(value)?);
        } else if args[i] == "--no-default-features" {
            no_default_features = true;
        } else {
            rest.push(args[i].clone());
        }
        i += 1;
    }
    Ok((features, no_default_features, rest))
}

pub(crate) fn extract_suite_selection(
    args: &[String],
) -> Result<(SuiteSelection, Vec<String>), CargoMooseError> {
    let mut sel = SuiteSelection::default();
    let mut remaining = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--suite" => {
                i += 1;
                let v = args.get(i).ok_or("--suite requires a name")?;
                sel.only_suites.push(v.clone());
            }
            "--no-suite" => sel.no_suite = true,
            "--no-per-plugin" => sel.no_per_plugin = true,
            other => remaining.push(other.to_string()),
        }
        i += 1;
    }
    if sel.no_suite && !sel.only_suites.is_empty() {
        return Err("--no-suite and --suite <name> are contradictory".into());
    }
    if sel.no_suite && sel.no_per_plugin {
        return Err("--no-suite and --no-per-plugin together produce no output".into());
    }
    Ok((sel, remaining))
}

/// Parsed format flags for the package command.
/// Used by both `cmd_package_macos` and `windows::cmd_package_windows`.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum PkgFormat {
    Clap,
    Vst3,
    /// Standalone host application built from the plugin's
    /// `[features].standalone`. Installs to `/Applications/` on macOS,
    /// `%PROGRAMFILES%\<Vendor>\<Plugin>\` on Windows, `/usr/bin/` (or
    /// user equivalent) on Linux.
    Standalone,
}

impl std::str::FromStr for PkgFormat {
    type Err = CargoMooseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "clap" => Ok(PkgFormat::Clap),
            "vst3" => Ok(PkgFormat::Vst3),
            "standalone" => Ok(PkgFormat::Standalone),
            other => Err(format!("unknown format: {other}").into()),
        }
    }
}

/// Per-format static metadata. One row per `PkgFormat` variant; adding
/// a new format means adding one row to [`PKG_FORMAT_META`] plus a
/// matching enum variant + `FromStr` arm.
///
/// Cross-platform rows. `extension`, `install_location`, etc. are
/// macOS-specific values (the `pkgbuild` / `productbuild` pipeline);
/// they're harmless string literals on Linux / Windows and the methods
/// that read them are gated on `cfg(target_os = "macos")`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct PkgFormatMeta {
    label: &'static str,
    pkg_id_suffix: &'static str,
    extension: &'static str,
    install_location: &'static str,
    is_native_bundle: bool,
    choice_description: &'static str,
}

const PKG_FORMAT_META: [(PkgFormat, PkgFormatMeta); 3] = [
    (
        PkgFormat::Clap,
        PkgFormatMeta {
            label: "CLAP",
            pkg_id_suffix: "clap",
            extension: "clap",
            install_location: "/Library/Audio/Plug-Ins/CLAP/",
            // macOS CLAP is a loadable bundle (`Contents/MacOS/<name>`
            // + `Info.plist`) - pkgbuild gets the same component-plist
            // treatment as VST3 so the installer pins it to the
            // declared install_location and upgrades by bundle ID.
            is_native_bundle: true,
            choice_description: "For Reaper, Bitwig",
        },
    ),
    (
        PkgFormat::Vst3,
        PkgFormatMeta {
            label: "VST3",
            pkg_id_suffix: "vst3",
            extension: "vst3",
            install_location: "/Library/Audio/Plug-Ins/VST3/",
            is_native_bundle: true,
            choice_description: "For Ableton, FL Studio, Reaper, Cubase",
        },
    ),
    (
        PkgFormat::Standalone,
        PkgFormatMeta {
            label: "Standalone",
            pkg_id_suffix: "standalone",
            extension: "app",
            install_location: "/Applications/",
            is_native_bundle: true,
            choice_description: "Run as a desktop app (no DAW required)",
        },
    ),
];

impl PkgFormat {
    /// Comma-separated list parser. Each token is fed through
    /// `PkgFormat::from_str` (the `FromStr` impl above), so an
    /// unknown token surfaces a "unknown format: …" error rather
    /// than a generic parse failure.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) fn parse_list(s: &str) -> Result<Vec<PkgFormat>, CargoMooseError> {
        s.split(',').map(|t| t.trim().parse()).collect()
    }

    fn meta(&self) -> &'static PkgFormatMeta {
        &PKG_FORMAT_META
            .iter()
            .find(|(f, _)| f == self)
            .expect("PKG_FORMAT_META is exhaustive over PkgFormat")
            .1
    }

    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) fn label(&self) -> &'static str {
        self.meta().label
    }
}

// macOS-only `pkgbuild` / `productbuild` plumbing. Windows packaging
// drives Inno Setup directly and doesn't need any of this.
#[cfg(target_os = "macos")]
impl PkgFormat {
    pub(crate) fn extension(&self) -> &'static str {
        self.meta().extension
    }

    pub(crate) fn install_location(&self) -> &'static str {
        self.meta().install_location
    }

    pub(crate) fn pkg_id_suffix(&self) -> &'static str {
        self.meta().pkg_id_suffix
    }

    /// Whether pkgbuild recognizes this as a native macOS bundle type.
    /// If false, we use --root instead of --component.
    pub(crate) fn is_native_bundle(&self) -> bool {
        self.meta().is_native_bundle
    }

    /// Bundle directory name for a given plugin.
    pub(crate) fn bundle_name(&self, plugin: &PluginDef) -> String {
        match self {
            // Plain `<Plugin>.app` so Spotlight / Launch Services index
            // it as a regular application. The historical
            // `<Plugin>.standalone.app` extension confused some indexing
            // paths and the bundle would not appear in Spotlight search.
            PkgFormat::Standalone => format!("{}.app", plugin.file_stem()),
            _ => format!("{}.{}", plugin.file_stem(), self.extension()),
        }
    }

    pub(crate) fn choice_description(&self) -> &'static str {
        self.meta().choice_description
    }

    /// True for formats whose install destination can't be redirected
    /// into the user's home - a standalone `.app` belongs in
    /// `/Applications/` to show up in Launchpad. When the user picks
    /// "Install for me only" but selects one of these, the installer escalates
    /// (`auth="Root"` on the corresponding `<pkg-ref>`) so the
    /// component still lands in the right place rather than failing
    /// with a permission-denied shove.
    pub(crate) fn is_system_only_on_macos(&self) -> bool {
        matches!(self, PkgFormat::Standalone)
    }
}

// `args` is unused on platforms where the body falls through to the
// "not supported" Err branch - silence the unused-variable warning
// only on those targets.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows")),
    allow(unused_variables)
)]
pub(crate) fn cmd_package(args: &[String]) -> Res {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    // Pull `--features` out first and set the invocation global, so it
    // reaches every build the fan-out spawns (per-OS, universal)
    // via `apply_extra_features` without each per-OS parser handling it.
    let (user_features, no_default_features, args) = extract_features_arg(args)?;
    crate::set_extra_features(user_features);
    // `--no-default-features` opts out of re-adding each plugin's
    // non-format default features (e.g. `ara`) to the per-format builds.
    crate::set_no_default_features(no_default_features);
    let (selection, args) = extract_suite_selection(&args)?;
    // Fail loudly on an unknown `--suite <name>` rather than silently
    // building only the per-plugin installers. Only load config when a
    // suite was explicitly requested.
    if !selection.only_suites.is_empty() {
        selection.validate_against(&crate::load_config()?.suites)?;
    }
    #[cfg(target_os = "windows")]
    {
        windows::cmd_package_windows(&args, &selection)
    }
    #[cfg(target_os = "macos")]
    {
        macos::cmd_package_macos(&args, &selection)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        linux::cmd_package_linux(&args, &selection)
    }
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo moose package [-p <crate>] [--suite <name>] \
[--no-suite|--no-per-plugin] [--formats <list>] \
[--user|--system|--ask] [--no-notarize] \
[--host-only|--universal]

Build, sign, and package plugins into installers. Per-plugin dist
filenames use the cargo crate name (e.g. `moose-example-gain`);
suite filenames use the suite's bundle_id. Same slug across
macOS / Windows / Linux:
  - macOS:   `target/dist/<crate>-<version>-macos.pkg` (productbuild)
  - Windows: `target/dist/<crate>-<version>-windows.exe` (Inno Setup)
  - Linux:   `target/dist/<crate>-<version>-linux-<arch>.tar.gz` + install.sh

Suites: declare one or more `[[suite]]` entries in moose.toml to bundle
multiple plugins into a single installer per platform. With suites
declared, `cargo moose package` produces both per-plugin installers and
per-suite installers by default. See docs for the schema.

Selection (composable):
  -p <crate>           Package only this plugin crate's installer.
  --suite <name>       Package only this suite (repeatable).
  --no-suite           Skip suite installers.
  --no-per-plugin      Skip per-plugin installers.

Format selection:
  --formats <list>     Comma-separated subset
                       (clap,vst3,standalone).
                       Default: every format in the plugin's
                       `[features].default`.
  --features <list>    Extra Cargo features for the plugin crate,
                       comma/space-separated. Additive, applied to every
                       underlying build. Format features are reserved -
                       use --formats.
  --no-default-features
                       Don't re-add the plugin's non-format default
                       features (e.g. ara) to each per-format build.

Install scope (where the resulting installer puts files at the end user's machine):
  --ask                End user picks at install time. Default.
  --user               User-scope. CLAP/VST3 land in user paths with no
                       admin prompt.
  --system             Hard-lock to system paths.
  Override the default project-wide via `[packaging] preferred_scope` in moose.toml.

Signing / notarization (macOS / Windows):
  --no-notarize        Skip macOS notarization (still codesigns).

Build target (macOS):
  --host-only          Single-arch build of the host. Default is universal.
  --universal          Explicit universal (no-op; same as default).

Build invocation (Linux):
  --no-build           Skip the implicit `cargo moose build` and use the
                       existing `target/bundles/` manifest as-is. Errors
                       if no manifest is present.

Codegen tuning (all platforms):
  --target-cpu <value> Override the x86_64 default of `-C target-cpu=x86-64-v3`.
                       baseline|v2|v3|v4|native or any literal rustc
                       target-cpu name. See `cargo moose build --help`
                       for the full description.

Misc:
  -h, --help           Show this message."
    );
}

#[cfg(test)]
mod selection_tests {
    use super::extract_suite_selection;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn empty_args_default_selection() {
        let (sel, rest) = extract_suite_selection(&[]).unwrap();
        assert!(sel.want_per_plugin());
        assert!(sel.want_suite("Studio", "studio"));
        assert!(rest.is_empty());
    }

    #[test]
    fn suite_filter_keeps_only_named_suites() {
        let (sel, rest) =
            extract_suite_selection(&s(&["--suite", "studio", "--suite", "free"])).unwrap();
        assert!(sel.want_suite("Studio", "studio"));
        assert!(sel.want_suite("Free", "free"));
        assert!(!sel.want_suite("MIDI Tools", "midi-tools"));
        assert!(rest.is_empty());
    }

    #[test]
    fn suite_filter_matches_bundle_id_or_name() {
        // `--suite` accepts the bundle_id slug or the display name.
        let (by_slug, _) = extract_suite_selection(&s(&["--suite", "moose-examples"])).unwrap();
        assert!(by_slug.want_suite("Moose Examples", "moose-examples"));
        let (by_name, _) = extract_suite_selection(&s(&["--suite", "Moose Examples"])).unwrap();
        assert!(by_name.want_suite("Moose Examples", "moose-examples"));
    }

    #[test]
    fn no_suite_drops_every_suite() {
        let (sel, _) = extract_suite_selection(&s(&["--no-suite"])).unwrap();
        assert!(!sel.want_suite("Studio", "studio"));
        assert!(sel.want_per_plugin());
    }

    #[test]
    fn no_per_plugin_keeps_suites() {
        let (sel, _) = extract_suite_selection(&s(&["--no-per-plugin"])).unwrap();
        assert!(!sel.want_per_plugin());
        assert!(sel.want_suite("Studio", "studio"));
    }

    #[test]
    fn other_flags_pass_through() {
        let (sel, rest) = extract_suite_selection(&s(&[
            "-p",
            "moose-gain",
            "--suite",
            "studio",
            "--no-notarize",
        ]))
        .unwrap();
        assert_eq!(rest, s(&["-p", "moose-gain", "--no-notarize"]));
        assert!(sel.want_suite("Studio", "studio"));
    }

    #[test]
    fn no_suite_and_no_per_plugin_together_errors() {
        let err = extract_suite_selection(&s(&["--no-suite", "--no-per-plugin"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no output"), "got: {err}");
    }

    #[test]
    fn no_suite_with_explicit_suite_errors() {
        let err = extract_suite_selection(&s(&["--no-suite", "--suite", "x"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("contradictory"), "got: {err}");
    }
}
