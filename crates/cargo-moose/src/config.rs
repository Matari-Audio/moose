//! Project configuration (read from `moose.toml`).
//!
//! `moose.toml` carries **project-level** facts only: vendor info,
//! plugin definitions, suite definitions, and packaging metadata
//! (publisher name, license file, installer icon).
//!
//! **Per-developer credentials and machine-specific paths
//! (signing identities, notarization Apple ID /
//! team ID, Authenticode certs) live in `.cargo/config.toml`'s
//! `[env]` table.** Cargo injects those into the environment before
//! invoking `cargo moose`, so the resolvers below just read
//! `std::env::var`. A direct-read fallback (`read_cargo_config_env`)
//! covers the rare case where `cargo-moose` runs outside cargo.
//!
//! There is no moose.toml-side option for any of these - by design.
//! The split keeps secrets out of the tracked file and removes the
//! "which copy wins?" question every time a developer onboards.

use crate::{CargoMooseError, project_root};
use serde::Deserialize;
use std::fs;

#[derive(Deserialize)]
pub(crate) struct Config {
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) macos: MacosConfig,
    #[serde(default)]
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) windows: WindowsConfig,
    pub(crate) vendor: VendorConfig,
    pub(crate) plugin: Vec<PluginDef>,
    /// Packaging metadata (welcome HTML, license HTML, etc.). Consumed
    /// by `cmd_package_macos` only - Windows packaging uses
    /// `WindowsConfig::packaging`, Linux has no packaging path.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) packaging: PackagingConfig,
    /// Suite installers - repeatable. Each entry produces one
    /// installer per platform that bundles the listed plugins.
    /// Empty = per-plugin output only (today's behaviour).
    #[serde(default, rename = "suite")]
    pub(crate) suites: Vec<SuiteDef>,
    /// Exact VST3 class IDs resolved from `moose.toml`, kept outside
    /// the public shared `PluginDef` struct-literal shape.
    #[serde(skip)]
    vst3_class_ids: Vec<(String, [u8; 16])>,
}

impl Config {
    pub(crate) fn vst3_cid(&self, plugin: &PluginDef) -> [u8; 16] {
        let explicit = self
            .vst3_class_ids
            .iter()
            .find(|(crate_name, _)| crate_name == &plugin.crate_name)
            .map(|(_, class_id)| *class_id);
        let id = moose_build::plugin_id(&self.vendor.id, &plugin.bundle_id);
        moose_utils::state::resolve_vst3_cid(explicit, &id)
    }
}

#[derive(Deserialize, Default)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct WindowsConfig {
    #[serde(default)]
    pub(crate) packaging: WindowsPackagingConfig,
}

#[derive(Deserialize, Default)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct WindowsPackagingConfig {
    /// Publisher name shown in the installer and Apps & Features.
    /// Defaults to `[vendor].name` when absent.
    pub(crate) publisher: Option<String>,
    /// Publisher URL shown in the installer.
    /// Defaults to `[vendor].url` when absent.
    pub(crate) publisher_url: Option<String>,
    /// Installer-window icon (.ico, relative to workspace root).
    pub(crate) installer_icon: Option<String>,
    /// Welcome/finish wizard bitmap (.bmp, 164x314, relative to workspace root).
    pub(crate) welcome_bmp: Option<String>,
    /// License shown on the wizard's license page (.rtf or .txt).
    pub(crate) license_rtf: Option<String>,
    /// Override for the stable `AppId` Inno Setup uses to detect upgrades.
    /// Defaults to `{vendor_id}.{bundle_id}` when absent.
    pub(crate) app_id: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct MacosConfig {
    /// Notarization config - only the `cmd_package_macos` path reads
    /// these fields, so on Windows / Linux they're parsed-and-ignored.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) packaging: MacosPackagingConfig,
}

#[derive(Deserialize, Default)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct MacosPackagingConfig {
    /// Whether to run `xcrun notarytool submit` on the produced
    /// `.pkg`. Project-level decision (release vs. dev build);
    /// the credentials it uses come from env vars
    /// (`MOOSE_NOTARY_PROFILE` keychain profile, or
    /// `APPLE_ID` + `TEAM_ID` + `APP_SPECIFIC_PASSWORD`).
    #[serde(default)]
    pub(crate) notarize: bool,
    /// Welcome-page HTML for the productbuild Distribution wizard
    /// (relative to workspace root). macOS-only - Windows has its own
    /// `[windows.packaging] welcome_bmp` slot with a different file
    /// format (164x314 .bmp).
    pub(crate) welcome_html: Option<String>,
    /// License-page HTML for the productbuild Distribution wizard.
    /// macOS-only - Windows uses `[windows.packaging] license_rtf`.
    pub(crate) license_html: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct PackagingConfig {
    /// Default format list for `cargo moose package` when no
    /// `--formats` flag is passed. Cross-platform - both the macOS
    /// `.pkg` and Windows Inno Setup paths read it. Linux's tarball
    /// pipeline ignores it (Linux ships every default-feature format
    /// the plugin built, no opt-in).
    #[serde(default)]
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) formats: Vec<String>,
    /// Preferred scope for `cargo moose package`: `"user"`,
    /// `"system"`, or `"ask"`. Absent = `"ask"` (the indie-installer
    /// convention where the end user picks at install time). CLI
    /// flags (`--user` / `--system` / `--ask`) override.
    /// Linux has no packaging pipeline, so the field is read only on
    /// macOS / Windows.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) preferred_scope: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct VendorConfig {
    pub(crate) name: String,
    /// Reverse-DNS vendor identifier (e.g. `com.acme`). Used by macOS
    /// `CFBundleIdentifier` plists and Windows Inno Setup paths;
    /// Linux VST3 bundles don't include a plist, so the field looks
    /// dead there. Keep cfg-gated to silence the lint without changing
    /// the schema.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) id: String,
    /// Vendor website URL. Used by the Windows Inno Setup installer's
    /// "Publisher URL" field; unused on macOS.
    #[serde(default)]
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) url: Option<String>,
    pub(crate) au_manufacturer: String,
}

/// Install-time view of a `[[plugin]]` entry.
///
/// Wraps the shared `moose_build::PluginDef` schema (consumed by the
/// proc macros) and adds install-only fields (`au3_subtype`,
/// `au_tag`). `Deref` exposes the shared fields so call sites read
/// `p.name` / `p.bundle_id` directly without going through `p.shared`.
#[derive(Deserialize)]
pub(crate) struct PluginDef {
    #[serde(flatten)]
    pub(crate) shared: moose_build::PluginDef,
    #[serde(default)]
    pub(crate) au3_subtype: Option<String>,
    #[serde(default = "default_au_tag")]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) au_tag: String,
    /// Per-plugin Windows app icon (`.ico`, path relative to workspace
    /// root). Embedded as `RT_GROUP_ICON` in the standalone `.exe`.
    /// Distinct from `[windows.packaging] installer_icon` (Inno-wizard
    /// chrome): a vendor with one installer-window logo can still ship
    /// different per-product app icons.
    #[serde(default)]
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) windows_icon: Option<String>,
    /// Per-plugin macOS app icon (`.icns`, path relative to workspace
    /// root). Copied into the standalone `.app`'s `Contents/Resources/`
    /// and referenced by `CFBundleIconFile` so Finder, the Dock,
    /// Launchpad, and Spotlight pick it up. Linux uses `.desktop` +
    /// freedesktop icons - file formats don't survive a single
    /// cross-OS slot. macOS has no installer-chrome icon equivalent;
    /// `.pkg` files inherit Installer.app's icon by design.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) macos_icon: Option<String>,
}

impl std::ops::Deref for PluginDef {
    type Target = moose_build::PluginDef;
    fn deref(&self) -> &Self::Target {
        &self.shared
    }
}

impl PluginDef {
    pub(crate) fn resolved_fourcc(&self) -> &str {
        self.fourcc
            .as_deref()
            .or(self.au_subtype.as_deref())
            .expect("moose.toml: each [[plugin]] requires `fourcc` or `au_subtype`")
    }
    pub(crate) fn resolved_au_type(&self) -> &str {
        // Keep in sync with `moose-derive::plugin_info`. NoteEffect →
        // `aumi` (Apple's MIDI Processor); an audio effect that accepts
        // MIDI input → `aumf` (MusicEffect), since AU routes MIDI by
        // component type. `aumi` plugins declare no audio buses per
        // Apple spec.
        if let Some(t) = self.au_type.as_deref() {
            return t;
        }
        match self.category.as_str() {
            "instrument" => "aumu",
            "midi" | "note_effect" => "aumi",
            _ => {
                // `load_config` rejected contradictory MIDI keys, so
                // the resolver can't fail here; the `aufx` fallback
                // only guards hand-built test configs.
                let accepts_midi_in = moose_build::midi_wiring(
                    &self.category,
                    self.midi_input,
                    self.midi_output,
                    self.midi_input_ports,
                    self.midi_output_ports,
                )
                .is_ok_and(|w| w.accepts_midi_in);
                if accepts_midi_in { "aumf" } else { "aufx" }
            }
        }
    }
    /// AU v3 component subtype: `au3_subtype` override, else the shared
    /// fourcc (so v2 and v3 register under the same code by default).
    pub(crate) fn au3_sub(&self) -> &str {
        self.au3_subtype
            .as_deref()
            .unwrap_or(self.resolved_fourcc())
    }
    /// Filesystem-safe form of the plugin's display name. Use this
    /// for every path component derived from the name (bundle
    /// directories, executable filenames inside Mach-O bundles, the
    /// staged `{name}.{ext}` artefacts under `target/bundles/`). The
    /// raw `self.name` stays untouched and is what hosts / Info.plist
    /// keys / DAW browsers display - sanitisation only kicks in at
    /// the path-construction boundary so a name like
    /// "Moose Dry/Wet" still shows up as written but lands on disk
    /// as `Moose Dry-Wet.aaxplugin`.
    pub(crate) fn file_stem(&self) -> String {
        moose_utils::safe_filename(&self.name)
    }
    /// Name of the AU v3 containing `.app`. AU v3 app mode *is* the
    /// plugin's standalone host with the appex embedded, so the bundle
    /// is the same `{name}.app` the standalone produces - no separate
    /// `"{name} v3"` app. `au3_name` now only overrides the AU's
    /// host-facing display name (the appex component), not the bundle
    /// path. macOS-only - AU v3 only installs to `/Applications/` there.
    #[cfg(target_os = "macos")]
    pub(crate) fn au3_app_name(&self) -> String {
        moose_utils::safe_filename(&self.name)
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn fw_name(&self) -> String {
        // `load_config` validated the shape (non-empty ASCII), but
        // stay panic-free for hand-built defs in tests.
        let mut chars = self.bundle_id.chars();
        let cap = chars.next().map_or_else(String::new, |first| {
            format!("{}{}", first.to_uppercase(), chars.as_str())
        });
        format!("Moose{cap}AU")
    }
    /// Dylib filename stem derived from the crate name (hyphens → underscores).
    pub(crate) fn dylib_stem(&self) -> String {
        self.crate_name.replace('-', "_")
    }
}

fn default_au_tag() -> String {
    "Effects".to_string()
}

/// One `[[suite]]` entry from `moose.toml`. Bundles a subset of the
/// workspace's plugins into a single installer per platform.
///
/// Defaults: `plugins` omitted → all workspace plugins;
/// `version` omitted → workspace version. `plugins` and
/// `exclude_plugins` are mutually exclusive - supplying both is a
/// hard error caught at validation time.
#[derive(Deserialize, Debug)]
pub(crate) struct SuiteDef {
    pub(crate) name: String,
    pub(crate) bundle_id: String,
    /// Explicit plugin list. Names match `[[plugin]].crate` (or
    /// `[[plugin]].bundle_id` - both accepted). Omit for "all".
    #[serde(default)]
    pub(crate) plugins: Option<Vec<String>>,
    /// Plugins to exclude from the otherwise-implicit "all". Mutually
    /// exclusive with `plugins`.
    #[serde(default)]
    pub(crate) exclude_plugins: Option<Vec<String>>,
    /// Suite-level version. Falls back to `[workspace.package].version`.
    #[serde(default)]
    pub(crate) version: Option<String>,
    /// Display blurb in the installer welcome page (where supported).
    #[serde(default)]
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) description: Option<String>,
}

impl SuiteDef {
    /// Validate the suite against the workspace config. Returns the
    /// resolved plugin set + format set the suite should ship.
    ///
    /// `workspace_plugins` is the full `Config::plugin` slice. Plugin
    /// names in the suite's `plugins` / `exclude_plugins` fields can
    /// be either the cargo crate name (`[[plugin]].crate`) or the
    /// `bundle_id`; both forms resolve here.
    pub(crate) fn resolve<'a>(
        &'a self,
        workspace_plugins: &'a [PluginDef],
    ) -> Result<ResolvedSuite<'a>, CargoMooseError> {
        if self.plugins.is_some() && self.exclude_plugins.is_some() {
            return Err(format!(
                "[[suite]] '{}' sets both `plugins` and `exclude_plugins` - \
                 these are mutually exclusive",
                self.name,
            )
            .into());
        }

        let resolve_one = |needle: &str| -> Result<&'a PluginDef, CargoMooseError> {
            workspace_plugins
                .iter()
                .find(|p| p.crate_name == needle || p.bundle_id == needle)
                .ok_or_else(|| {
                    format!(
                        "[[suite]] '{}': plugin '{}' is not in the workspace. \
                         Available: {}",
                        self.name,
                        needle,
                        workspace_plugins
                            .iter()
                            .map(|p| p.crate_name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                    .into()
                })
        };

        let plugins: Vec<&PluginDef> = if let Some(list) = &self.plugins {
            list.iter()
                .map(|s| resolve_one(s))
                .collect::<Result<Vec<_>, _>>()?
        } else if let Some(excl) = &self.exclude_plugins {
            let exclude_set: Vec<&PluginDef> = excl
                .iter()
                .map(|s| resolve_one(s))
                .collect::<Result<Vec<_>, _>>()?;
            workspace_plugins
                .iter()
                .filter(|p| !exclude_set.iter().any(|e| std::ptr::eq(*e, *p)))
                .collect()
        } else {
            workspace_plugins.iter().collect()
        };

        if plugins.is_empty() {
            return Err(format!(
                "[[suite]] '{}' resolves to zero plugins after \
                 plugins/exclude_plugins resolution",
                self.name,
            )
            .into());
        }

        Ok(ResolvedSuite { def: self, plugins })
    }
}

/// Result of [`SuiteDef::resolve`]. Borrows from the original
/// workspace config so we don't clone every plugin per suite.
pub(crate) struct ResolvedSuite<'a> {
    pub(crate) def: &'a SuiteDef,
    pub(crate) plugins: Vec<&'a PluginDef>,
}

/// Read a per-developer build env var. Cargo injects values from
/// `.cargo/config.toml`'s `[env]` table into the environment of any
/// subcommand it spawns, so `std::env::var(key)` is the normal path.
/// As a fallback for the rare `cargo-moose` invocation that doesn't
/// go through cargo (e.g. running `target/release/cargo-moose`
/// directly), parse `.cargo/config.toml` ourselves.
///
/// Returns `None` for missing or empty values (an empty string from
/// either source is treated as unset). Cargo's `force = true`
/// override on a `[env]` entry is handled transparently because
/// cargo has already applied it to the process environment by the
/// time we read.
///
/// A `MOOSE_*` key falls back to the pre-fork `TRUCE_*` name (with a
/// one-time deprecation warning) so a truce `.cargo/config.toml` keeps
/// working.
pub(crate) fn read_build_env(key: &str) -> Option<String> {
    if let Some(v) = read_build_env_exact(key) {
        return Some(v);
    }
    let legacy = moose_utils::env::legacy_name(key)?;
    let v = read_build_env_exact(&legacy)?;
    moose_utils::env::warn_once(&legacy, key);
    Some(v)
}

fn read_build_env_exact(key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(key)
        && !v.is_empty()
    {
        return Some(v);
    }
    let root = project_root();
    let path = root.join(".cargo/config.toml");
    let content = fs::read_to_string(&path).ok()?;
    let doc: toml::Table = content.parse().ok()?;
    let env = doc.get("env")?.as_table()?;
    // Supports both `KEY = "value"` and `KEY = { value = "...", force = true }`.
    let raw = match env.get(key)? {
        toml::Value::String(s) => s.clone(),
        toml::Value::Table(t) => t.get("value")?.as_str()?.to_string(),
        _ => return None,
    };
    if raw.is_empty() { None } else { Some(raw) }
}

/// Resolved application signing identity. `"-"` means ad-hoc /
/// unsigned (the default). Read from the `MOOSE_SIGNING_IDENTITY`
/// build env. The accessor stays in this module so all callers have
/// one path to follow when they need to know "where does this come
/// from?"
pub(crate) fn application_identity() -> String {
    read_build_env("MOOSE_SIGNING_IDENTITY").unwrap_or_else(|| "-".to_string())
}

/// Resolved installer signing identity. `None` means the installer
/// won't be signed. Read from the `MOOSE_INSTALLER_SIGNING_IDENTITY`
/// build env. macOS-only - only the `productbuild` step in
/// `cmd_package_macos` consumes this.
#[cfg(target_os = "macos")]
pub(crate) fn installer_identity() -> Option<String> {
    read_build_env("MOOSE_INSTALLER_SIGNING_IDENTITY")
}

/// Read `MACOSX_DEPLOYMENT_TARGET` from the build env, defaulting
/// to "11.0".
pub(crate) fn deployment_target() -> String {
    read_build_env("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|| "11.0".to_string())
}

/// One deprecation line per process, however many times the config is
/// loaded.
fn warn_legacy_config_once(path: &std::path::Path) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "warning: reading {} - `truce.toml` is deprecated, rename it to `moose.toml`",
            path.display()
        );
    });
}

pub(crate) fn load_config() -> std::result::Result<Config, CargoMooseError> {
    let root = project_root();
    let Some(path) = moose_build::config_file_in(&root) else {
        return Err(format!(
            "moose.toml not found in {}. Run 'cargo moose new' to scaffold a project, or create moose.toml manually.",
            root.display()
        )
        .into());
    };
    if path.ends_with(moose_build::LEGACY_CONFIG_FILE) {
        warn_legacy_config_once(&path);
    }
    let content = fs::read_to_string(&path)?;
    let mut config: Config = toml::from_str(&content)?;
    if config.plugin.is_empty() {
        return Err("No [[plugin]] entries in moose.toml".into());
    }
    // Reject contradictory MIDI keys here so every install/package/
    // validate path fails with the plugin named, mirroring the
    // compile error moose-derive raises for the same config.
    let vst3_class_ids = moose_build::load_vst3_class_ids(&path)?;
    for p in &config.plugin {
        if let Err(msg) = moose_build::validate_bundle_id(&p.bundle_id) {
            return Err(format!("[[plugin]] `{}`: {msg}", p.crate_name).into());
        }
        let check = moose_build::midi_wiring(
            &p.category,
            p.midi_input,
            p.midi_output,
            p.midi_input_ports,
            p.midi_output_ports,
        )
        .and_then(|wiring| {
            moose_build::midi2_dialects(&wiring, p.midi2, p.midi2_input, p.midi2_output)
        });
        if let Err(msg) = check {
            return Err(format!("[[plugin]] `{}`: {msg}", p.crate_name).into());
        }
    }
    config.vst3_class_ids = vst3_class_ids;
    Ok(config)
}

#[cfg(test)]
mod suite_tests {
    use super::*;

    fn plugin(crate_name: &str, bundle_id: &str) -> PluginDef {
        PluginDef {
            shared: moose_build::PluginDef {
                name: crate_name.into(),
                bundle_id: bundle_id.into(),
                crate_name: crate_name.into(),
                version: None,
                description: None,
                fourcc: None,
                category: "effect".into(),
                au_type: None,
                au_subtype: None,
                vst3_subcategory: None,
                vst3_name: None,
                clap_name: None,
                clap_manual_url: None,
                clap_support_url: None,
                clap_features: Vec::new(),
                au_name: None,
                au3_name: None,
                mute_preview_output: false,
                midi_input: None,
                midi_output: None,
                midi2: false,
                midi2_input: None,
                midi2_output: None,
                midi_input_ports: None,
                midi_output_ports: None,
                presets: None,
                legacy_state: None,
            },
            au3_subtype: None,
            au_tag: default_au_tag(),
            windows_icon: None,
            macos_icon: None,
        }
    }

    fn suite(name: &str) -> SuiteDef {
        SuiteDef {
            name: name.into(),
            bundle_id: name.to_lowercase(),
            plugins: None,
            exclude_plugins: None,
            version: None,
            description: None,
        }
    }

    #[test]
    fn default_resolves_to_all_workspace_plugins() {
        let plugins = vec![plugin("a", "a"), plugin("b", "b"), plugin("c", "c")];
        let s = suite("Studio");
        let r = match s.resolve(&plugins) {
            Ok(r) => r,
            Err(e) => panic!("resolve failed: {e}"),
        };
        assert_eq!(r.plugins.len(), 3);
    }

    #[test]
    fn explicit_plugin_list_narrows() {
        let plugins = vec![plugin("a", "a"), plugin("b", "b"), plugin("c", "c")];
        let mut s = suite("Studio");
        s.plugins = Some(vec!["a".into(), "c".into()]);
        let r = match s.resolve(&plugins) {
            Ok(r) => r,
            Err(e) => panic!("resolve failed: {e}"),
        };
        let names: Vec<_> = r.plugins.iter().map(|p| p.crate_name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn exclude_plugins_inverts() {
        let plugins = vec![plugin("a", "a"), plugin("b", "b"), plugin("c", "c")];
        let mut s = suite("Studio");
        s.exclude_plugins = Some(vec!["b".into()]);
        let r = match s.resolve(&plugins) {
            Ok(r) => r,
            Err(e) => panic!("resolve failed: {e}"),
        };
        let names: Vec<_> = r.plugins.iter().map(|p| p.crate_name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn bundle_id_resolves_alongside_crate_name() {
        let plugins = vec![plugin("acme-gain", "gain")];
        let mut s = suite("Studio");
        // Reference by bundle_id rather than crate name.
        s.plugins = Some(vec!["gain".into()]);
        let r = match s.resolve(&plugins) {
            Ok(r) => r,
            Err(e) => panic!("resolve failed: {e}"),
        };
        assert_eq!(r.plugins.len(), 1);
    }

    #[test]
    fn unknown_plugin_errors() {
        let plugins = vec![plugin("a", "a")];
        let mut s = suite("Studio");
        s.plugins = Some(vec!["does-not-exist".into()]);
        let err = match s.resolve(&plugins) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected resolve to error"),
        };
        assert!(err.contains("does-not-exist"), "got: {err}");
        assert!(err.contains("Studio"), "got: {err}");
    }

    #[test]
    fn plugins_and_exclude_plugins_both_set_errors() {
        let plugins = vec![plugin("a", "a")];
        let mut s = suite("Studio");
        s.plugins = Some(vec!["a".into()]);
        s.exclude_plugins = Some(vec!["a".into()]);
        let err = match s.resolve(&plugins) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected resolve to error"),
        };
        assert!(err.contains("mutually exclusive"));
    }

    #[test]
    fn empty_resolution_errors() {
        // Three plugins, exclude all three → zero remaining.
        let plugins = vec![plugin("a", "a"), plugin("b", "b")];
        let mut s = suite("Studio");
        s.exclude_plugins = Some(vec!["a".into(), "b".into()]);
        let err = match s.resolve(&plugins) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected resolve to error"),
        };
        assert!(err.contains("zero plugins"), "got: {err}");
    }

    #[test]
    fn au_type_promotes_midi_effect_to_aumf() {
        let mut p = plugin("fx", "fx");
        // Plain audio effect stays aufx.
        assert_eq!(p.resolved_au_type(), "aufx");
        // Opting into MIDI input promotes it to MusicEffect.
        p.shared.midi_input = Some(true);
        assert_eq!(p.resolved_au_type(), "aumf");
        // An explicit au_type override still wins.
        p.shared.au_type = Some("aufx".into());
        assert_eq!(p.resolved_au_type(), "aufx");
    }

    #[test]
    fn au_type_category_defaults() {
        let mut p = plugin("p", "p");
        p.shared.category = "instrument".into();
        assert_eq!(p.resolved_au_type(), "aumu");
        p.shared.category = "note_effect".into();
        assert_eq!(p.resolved_au_type(), "aumi");
    }
}
