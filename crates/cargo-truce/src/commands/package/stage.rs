//! Format-specific staging: copy the built dylib into a bundle layout,
//! write the per-format Info.plist, and codesign.

#[cfg(target_os = "macos")]
use super::PkgFormat;
use crate::commands::install::presets;
#[cfg(target_os = "macos")]
use crate::install_scope::PkgScope;
// Every `xml_escape` call site here is a macOS plist writer.
#[cfg(target_os = "macos")]
use crate::preset_codec::xml_escape;
use crate::{Config, PluginDef, Res};
#[cfg(target_os = "macos")]
use crate::{MacosPackagingConfig, codesign_bundle};
#[cfg(target_os = "macos")]
use std::fmt::Write;
use std::fs;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;

/// Build a standalone pkg component for an out-of-bundle preset
/// payload: stage `payload` (relative path -> bytes) under a private
/// pkgroot, then `pkgbuild` it with `install_location` as the target.
/// Used for VST3 presets, which install to the OS preset folder rather
/// than the plugin bundle. Produces `<file_stem>-<label>.pkg` in
/// `components_dir`. No `--scripts`: the preset dir is shared with
/// user / host presets, so the component only adds its own files (BOM
/// removal on upgrade leaves user presets untouched) and never wipes.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_preset_component(
    staging: &Path,
    components_dir: &Path,
    file_stem: &str,
    pkg_id: &str,
    label: &str,
    install_location: &str,
    version: &str,
    payload: &[(PathBuf, Vec<u8>)],
) -> Res {
    let root = staging.join(format!("_presetroot_{label}"));
    let _ = fs::remove_dir_all(&root);
    for (rel, bytes) in payload {
        let dst = root.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dst, bytes)?;
    }
    let component_pkg = components_dir.join(format!("{file_stem}-{label}.pkg"));
    let status = Command::new("pkgbuild")
        .args([
            "--root",
            root.to_str().unwrap(),
            "--install-location",
            install_location,
            "--identifier",
            pkg_id,
            "--version",
            version,
            "--ownership",
            "preserve",
            component_pkg.to_str().unwrap(),
        ])
        .status()?;
    if !status.success() {
        return Err(format!("pkgbuild failed for {file_stem} {label}").into());
    }
    Ok(())
}

/// Stage a CLAP bundle into the staging directory. `target` selects
/// which `target/<triple>/release/` to read from (`None` = host's
/// `target/release/`).
///
/// macOS uses the loadable-bundle layout that hosts like Bitwig expect
/// (`{name}.clap/Contents/MacOS/<name>` + `Info.plist`). Linux and
/// Windows keep the flat `.so` / `.dll` renamed `.clap`.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub(crate) fn stage_clap(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    staging: &Path,
    identity: &str,
    target: Option<&str>,
) -> Res {
    // Branch on the *target* OS, not the host: macOS ships an
    // `MH_BUNDLE` `.clap` directory, Linux / Windows a single-file
    // `.clap` (the cdylib renamed). A cross build must lay out the
    // target's shape, not the host's.
    let triple = target.unwrap_or_else(|| truce_build::host_triple());
    let bundle = staging.join(format!("{}.clap", p.file_stem()));
    if crate::target_os_of(triple) == "macos" {
        stage_clap_macos(root, p, config, &bundle, identity)
    } else {
        stage_clap_shared_lib(root, p, config, staging, &bundle, target)
    }
}

/// macOS `.clap`: an `MH_BUNDLE` under `Contents/MacOS` + plist + sealed
/// factory presets + codesign. Needs a macOS host (`clang -bundle` +
/// `codesign`).
#[cfg(target_os = "macos")]
fn stage_clap_macos(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    bundle: &Path,
    identity: &str,
) -> Res {
    let dylib = crate::release_bundle_bin(root, &p.dylib_stem(), "_clap");
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let macos_dir = bundle.join("Contents/MacOS");
    fs::create_dir_all(&macos_dir)?;
    let exec_name = p.file_stem();
    fs::copy(&dylib, macos_dir.join(&exec_name))?;

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
        display_name = xml_escape(&p.name),
        bundle_id = p.bundle_id,
        vendor_id = xml_escape(&config.vendor.id),
        exec_name = xml_escape(&exec_name),
    );
    fs::write(bundle.join("Contents/Info.plist"), &plist)?;
    // Presets are part of the bundle's sealed Resources - emit before
    // codesign so the signature covers them.
    if let Some(fp) = presets::load_factory_presets(root, p, config)? {
        presets::emit_trucepreset_tree(
            &fp,
            &bundle.join("Contents/Resources/Presets"),
            false,
            &format!("{}-clap", p.bundle_id),
        )?;
    }
    codesign_bundle(bundle.to_str().unwrap(), identity, false)?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn stage_clap_macos(
    _root: &Path,
    _p: &PluginDef,
    _config: &Config,
    _bundle: &Path,
    _identity: &str,
) -> Res {
    Err("cargo-truce: building a macOS CLAP bundle requires a macOS host".into())
}

/// Single-file `.clap` for Linux / Windows: the cdylib renamed to
/// `<name>.clap`, with factory presets in a `<name>.presets/` sibling
/// where the discovery provider looks. No codesign (not a macOS
/// bundle), so it runs on any build host - including a macOS cross build.
fn stage_clap_shared_lib(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    staging: &Path,
    bundle: &Path,
    target: Option<&str>,
) -> Res {
    let dylib = crate::release_lib_for_target(root, &format!("{}_clap", p.dylib_stem()), target);
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    fs::copy(&dylib, bundle)?;
    if let Some(fp) = presets::load_factory_presets(root, p, config)? {
        presets::emit_trucepreset_tree(
            &fp,
            &staging.join(format!("{}.presets", p.file_stem())),
            false,
            &format!("{}-clap", p.bundle_id),
        )?;
    }
    Ok(())
}

/// Stage a VST3 bundle into the staging directory. `target` selects
/// which `target/<triple>/release/` to read from (`None` = host's
/// `target/release/`) and also drives the VST3 inner-arch subdir
/// (`Contents/x86_64-linux/`, `Contents/aarch64-linux/`, etc.).
pub(crate) fn stage_vst3(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    staging: &Path,
    target: Option<&str>,
) -> Res {
    // VST3 bundle layout is platform-specific (Steinberg "Bundle Locations"):
    //   macOS:   Contents/MacOS/<name>             (Mach-O, no extension)
    //   Linux:   Contents/<arch>-linux/<name>.so   (ELF, .so)
    //   Windows: Contents/<arch>-win/<name>.vst3   (PE, .vst3)
    // Branch on the *target* OS, not the host, so a cross build lays out
    // the bundle for where it will load.
    let triple = target.unwrap_or_else(|| truce_build::host_triple());
    let bundle = staging.join(format!("{}.vst3", p.file_stem()));
    if crate::target_os_of(triple) == "macos" {
        stage_vst3_macos(root, p, config, &bundle)
    } else {
        stage_vst3_shared_lib(root, p, &bundle, triple, target)
    }
}

/// macOS `.vst3`: an `MH_BUNDLE` under `Contents/MacOS` + plist +
/// codesign. Needs a macOS host.
#[cfg(target_os = "macos")]
fn stage_vst3_macos(root: &Path, p: &PluginDef, config: &Config, bundle: &Path) -> Res {
    let dylib = crate::release_bundle_bin(root, &p.dylib_stem(), "_vst3");
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let macos_dir = bundle.join("Contents/MacOS");
    fs::create_dir_all(&macos_dir)?;
    let exec_name = p.file_stem();
    fs::copy(&dylib, macos_dir.join(&exec_name))?;

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
        display_name = xml_escape(&p.name),
        bundle_id = p.bundle_id,
        vendor_id = xml_escape(&config.vendor.id),
        exec_name = xml_escape(&exec_name),
    );
    fs::write(bundle.join("Contents/Info.plist"), &plist)?;
    codesign_bundle(
        bundle.to_str().unwrap(),
        &crate::application_identity(),
        false,
    )?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn stage_vst3_macos(_root: &Path, _p: &PluginDef, _config: &Config, _bundle: &Path) -> Res {
    Err("cargo-truce: building a macOS VST3 bundle requires a macOS host".into())
}

/// Linux / Windows `.vst3`: the cdylib into
/// `Contents/<arch>-{linux,win}/<name>.{so,vst3}` per the SDK "Bundle
/// Locations" spec. No macOS host tools, so it runs on any build host.
fn stage_vst3_shared_lib(
    root: &Path,
    p: &PluginDef,
    bundle: &Path,
    triple: &str,
    target: Option<&str>,
) -> Res {
    let dylib = crate::release_lib_for_target(root, &format!("{}_vst3", p.dylib_stem()), target);
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let arch_dir = bundle.join("Contents").join(vst3_arch_subdir(triple));
    fs::create_dir_all(&arch_dir)?;
    let inner_filename = format!("{}.{}", p.file_stem(), vst3_inner_extension(triple));
    fs::copy(&dylib, arch_dir.join(inner_filename))?;
    Ok(())
}

/// VST3 bundle inner-directory name per the VST3 SDK "Bundle Locations"
/// spec. Maps a cargo target triple to the bundle's `Contents/<dir>/`.
/// macOS callers don't reach this - they use the special `MacOS` dir.
fn vst3_arch_subdir(triple: &str) -> &'static str {
    match triple {
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => "x86_64-linux",
        "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl" => "aarch64-linux",
        "x86_64-pc-windows-msvc" | "x86_64-pc-windows-gnu" => "x86_64-win",
        "aarch64-pc-windows-msvc" => "aarch64-win",
        // Linux/Windows on a non-mainstream arch - VST3 hosts on those
        // arches wouldn't load it anyway. Emit something deterministic
        // so the bundle structure stays parseable.
        _ => "unknown",
    }
}

/// VST3 inner-binary extension per the VST3 SDK spec. Linux uses
/// `.so`; Windows uses `.vst3`.
fn vst3_inner_extension(triple: &str) -> &'static str {
    if triple.contains("linux") {
        "so"
    } else if triple.contains("windows") {
        "vst3"
    } else {
        "so"
    }
}

/// Stage the standalone host as a `.app` bundle inside the packaging
/// staging tree. Reads the per-arch standalone binaries built by
/// `build_and_lipo_standalone`, lipo-merges (or copies, single-arch)
/// into `<staging>/<Plugin>.app/Contents/MacOS/<bin>`, writes the
/// Info.plist, and codesigns. The pkgbuild step downstream installs
/// the resulting `.app` to `/Applications/`.
#[cfg(target_os = "macos")]
pub(crate) fn stage_standalone(root: &Path, p: &PluginDef, config: &Config, staging: &Path) -> Res {
    use std::os::unix::fs::PermissionsExt;

    let bin_stem = crate::read_standalone_bin_name(&p.crate_name)
        .unwrap_or_else(|| format!("{}-standalone", p.crate_name));

    // Universal output written by `build_and_lipo_standalone` to
    // `target/release/<bin_stem>` (single-arch falls through to the
    // same path via `cp`).
    let built = truce_build::target_dir(root)
        .join("release")
        .join(&bin_stem);
    if !built.exists() {
        return Err(format!(
            "Standalone binary missing at {}. \
             The build step should have produced it - make sure the \
             plugin's Cargo.toml declares a [[bin]] target named '{}'.",
            built.display(),
            bin_stem,
        )
        .into());
    }

    let staged_app = staging.join(format!("{}.app", p.file_stem()));
    let _ = fs::remove_dir_all(&staged_app);
    let macos_dir = staged_app.join("Contents/MacOS");
    fs::create_dir_all(&macos_dir)?;
    let exe_dst = macos_dir.join(&bin_stem);
    fs::copy(&built, &exe_dst)?;

    // Mark the binary executable. `pkgbuild` preserves the staged
    // mode bits; without this the installed app refuses to launch
    // ("permission denied") on the end user's machine.
    let mut perms = fs::metadata(&exe_dst)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&exe_dst, perms)?;

    // Optional per-plugin app icon. Drop the `.icns` into
    // `Contents/Resources/icon.icns` and let `write_standalone_info_plist`
    // emit the matching `CFBundleIconFile` key. Absent = no icon
    // (system default folder-with-cog).
    let icon_present = if let Some(icon_rel) = &p.macos_icon {
        let icon_src = crate::project_root().join(icon_rel);
        if !icon_src.exists() {
            return Err(format!(
                "macos_icon for `{}` points to {} but no file is there.",
                p.name,
                icon_src.display()
            )
            .into());
        }
        let resources_dir = staged_app.join("Contents/Resources");
        fs::create_dir_all(&resources_dir)?;
        fs::copy(&icon_src, resources_dir.join("icon.icns"))?;
        true
    } else {
        false
    };

    write_standalone_info_plist(&staged_app, p, &bin_stem, &config.vendor, icon_present)?;

    // Factory presets into Contents/Resources/Presets, before
    // codesign so the seal covers them. The installed app resolves
    // them through its own `installed_factory_root`.
    crate::commands::install::presets::emit_standalone_factory(root, p, config, &exe_dst)?;

    codesign_bundle(
        staged_app.to_str().unwrap(),
        &crate::application_identity(),
        false,
    )?;

    Ok(())
}

/// Write a `.app/Contents/Info.plist` for a standalone host bundle.
/// Shared between `commands::run` (dev iteration) and the packaging
/// pipeline so the live-run app and the installed app present
/// identically to the OS - same Dock name, same mic-permission prompt,
/// same hi-DPI flag.
#[cfg(target_os = "macos")]
pub(crate) fn write_standalone_info_plist(
    bundle_root: &Path,
    plugin: &PluginDef,
    bin_stem: &str,
    vendor: &crate::config::VendorConfig,
    icon_present: bool,
) -> Res {
    let mic_usage = format!(
        "{} would like to use the microphone for plugin audio input.",
        plugin.name
    );
    // Emit `CFBundleIconFile` only when the caller staged an
    // `icon.icns` next to Info.plist. macOS will otherwise scribble a
    // missing-resource error in the system log on first launch.
    let icon_key = if icon_present {
        "    <key>CFBundleIconFile</key>\n    <string>icon</string>\n"
    } else {
        ""
    };
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundleDisplayName</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{bundle_id}.standalone</string>
    <key>CFBundleExecutable</key>
    <string>{exe}</string>
{icon_key}    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>{mic_usage}</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.music</string>
</dict>
</plist>
"#,
        name = xml_escape(&plugin.name),
        vendor_id = xml_escape(&vendor.id),
        bundle_id = plugin.bundle_id,
        exe = xml_escape(bin_stem),
        mic_usage = xml_escape(&mic_usage),
    );
    fs::write(bundle_root.join("Contents/Info.plist"), plist)?;
    Ok(())
}

/// Generate the distribution.xml for the macOS .pkg installer.
#[cfg(target_os = "macos")]
/// A standalone pkg component carrying out-of-bundle payload (VST3
/// presets install to the OS preset folder, not the plugin bundle).
/// It needs its own pkg because the install-location differs from the
/// bundle, but it isn't a separate user choice: its `<pkg-ref>` rides
/// inside `parent`'s `<choice>`, so it installs whenever that format
/// does, and inherits that format's `auth` treatment.
#[cfg(target_os = "macos")]
pub(crate) struct ExtraComponent {
    /// pkg-id suffix, e.g. `vst3presets`.
    pub suffix: String,
    /// File-name segment, e.g. `VST3-Presets` (the component pkg is
    /// `<plugin>-<label>.pkg`).
    pub label: String,
    /// The format whose `<choice>` this component rides inside.
    pub parent: PkgFormat,
}

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn generate_distribution_xml(
    plugin_name: &str,
    vendor_id: &str,
    bundle_id: &str,
    formats: &[PkgFormat],
    extras: &[ExtraComponent],
    version: &str,
    resources: Option<&MacosPackagingConfig>,
    scope: PkgScope,
) -> String {
    let mut choices_outline = String::new();
    let mut choices = String::new();
    let mut pkg_refs = String::new();

    for fmt in formats {
        let id = fmt.pkg_id_suffix();
        let pkg_id = format!("{vendor_id}.{bundle_id}.{id}");
        let label = fmt.label();
        let (title, desc): (&str, &str) = (label, fmt.choice_description());
        let component_file = format!("{plugin_name}-{label}.pkg");

        // Every format ships checked by default.
        let enabled_attr = "";

        // Per-choice auth override. pkgbuild stamps every component
        // with `auth="root"` because the install-location sits under
        // `/Library/...` or `/Applications/`; left as-is the
        // installer's `shove` step tries to chown the payload to
        // `root:wheel` even when "Install for me only" relocated the
        // destination to the user's home, and fails with EACCES.
        //
        // - `--user` (explicit): user-viable formats (CLAP, VST3)
        //   override to `auth="None"` so the relocated
        //   `~/Library/Audio/Plug-Ins/...` install runs as the
        //   current user with no chown. System-only formats
        //   (standalone) keep `auth="Root"` so they escalate
        //   for `/Library/...` / `/Applications/`.
        // - `--ask` (default): leave user-viable formats at the
        //   component default - the user might pick "System" at
        //   install time, which needs root either way. System-only
        //   formats still get `auth="Root"` so they always escalate.
        // - `--system`: leave defaults; admin is needed regardless.
        let pkg_ref_auth = match (scope, fmt.is_system_only_on_macos()) {
            (PkgScope::User | PkgScope::Ask, true) => " auth=\"Root\"",
            (PkgScope::User, false) => " auth=\"None\"",
            (PkgScope::Ask | PkgScope::System, _) => "",
        };

        // Out-of-bundle components ride inside this format's choice
        // rather than as their own: VST3 presets install whenever VST3
        // does. They stay a separate *pkg* (their install-location is
        // the OS preset folder, not the bundle) but not a separate
        // user-facing choice. Same auth as the parent format.
        let mut extra_refs = String::new();
        for ec in extras.iter().filter(|e| e.parent == *fmt) {
            let ex_id = format!("{vendor_id}.{bundle_id}.{}", ec.suffix);
            let _ = writeln!(
                extra_refs,
                "        <pkg-ref id=\"{ex_id}\"{pkg_ref_auth}/>"
            );
        }

        let _ = writeln!(choices_outline, "        <line choice=\"{id}\"/>");
        let _ = write!(
            choices,
            r#"
    <choice id="{id}" title="{title}" description="{desc}"{enabled_attr}>
        <pkg-ref id="{pkg_id}"{pkg_ref_auth}/>
{extra_refs}    </choice>
"#
        );
        let _ = writeln!(
            pkg_refs,
            "    <pkg-ref id=\"{pkg_id}\" version=\"{version}\">{component_file}</pkg-ref>"
        );
        for ec in extras.iter().filter(|e| e.parent == *fmt) {
            let ex_id = format!("{vendor_id}.{bundle_id}.{}", ec.suffix);
            let ex_file = format!("{plugin_name}-{}.pkg", ec.label);
            let _ = writeln!(
                pkg_refs,
                "    <pkg-ref id=\"{ex_id}\" version=\"{version}\">{ex_file}</pkg-ref>"
            );
        }
    }

    let welcome = resources
        .and_then(|r| r.welcome_html.as_deref())
        .map_or("", |_| "    <welcome file=\"welcome.html\"/>\n");
    let license = resources
        .and_then(|r| r.license_html.as_deref())
        .map_or("", |_| "    <license file=\"license.html\"/>\n");

    // Per-scope <domains> drives Installer.app's "Destination Select"
    // page. `--ask` enables both - Installer.app shows the radio
    // buttons. `--user` / `--system` hard-lock the prefix, no page.
    let domains = match scope {
        PkgScope::User => {
            "    <domains enable_anywhere=\"false\" enable_currentUserHome=\"true\" \
             enable_localSystem=\"false\"/>\n"
        }
        PkgScope::System => {
            "    <domains enable_anywhere=\"false\" enable_currentUserHome=\"false\" \
             enable_localSystem=\"true\"/>\n"
        }
        PkgScope::Ask => {
            "    <domains enable_anywhere=\"false\" enable_currentUserHome=\"true\" \
             enable_localSystem=\"true\"/>\n"
        }
    };

    // Escape only now: the loop above used the raw name for `.pkg`
    // component filenames; the `<title>` needs it XML-safe.
    let plugin_name = xml_escape(plugin_name);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>{plugin_name}</title>
{welcome}{license}{domains}    <options customize="always" require-scripts="false"/>

    <choices-outline>
{choices_outline}    </choices-outline>
{choices}
{pkg_refs}</installer-gui-script>
"#
    )
}

/// Build per-format pkgbuild scripts under `staging/<fmt>_scripts/`
/// and return the directory path. Every format gets a `preinstall`
/// that removes any existing bundle at the destination before shove
/// runs - without this, a stale leftover (especially one owned by
/// root from a prior admin install) blocks the new payload with
/// `Permission denied` during the relink step.
///
/// The preinstall reads `$2` (the resolved install destination -
/// already accounts for `enable_currentUserHome` relocation) and
/// removes `<destination>/<bundle_name>` if present. When running
/// under root auth (`Install for all users` or a per-pkg-ref
/// `auth="Root"`) the rm succeeds regardless of leftover owner;
/// when running as the user the rm only works on user-owned
/// leftovers and fails loudly with an actionable message otherwise
/// (so the developer doing `cargo truce package --user` after a
/// `--system` round sees what to clean up).
#[cfg(target_os = "macos")]
pub(crate) fn write_format_scripts(
    staging: &Path,
    fmt: &PkgFormat,
    bundle_name: &str,
) -> std::result::Result<PathBuf, crate::CargoTruceError> {
    let scripts_dir = staging.join(format!("{}_scripts", fmt.pkg_id_suffix()));
    let _ = fs::remove_dir_all(&scripts_dir);
    fs::create_dir_all(&scripts_dir)?;

    let escaped_bundle = bundle_name.replace('"', "\\\"");
    let preinstall = scripts_dir.join("preinstall");
    fs::write(
        &preinstall,
        format!(
            "#!/bin/bash\n\
             # `cargo truce package` preinstall: remove any prior\n\
             # bundle at the destination before shove writes ours.\n\
             # `$2` is the resolved install destination (with\n\
             # `enable_currentUserHome` redirection applied).\n\
             set -u\n\
             BUNDLE=\"$2/{escaped_bundle}\"\n\
             if [ -e \"$BUNDLE\" ]; then\n    \
                 if rm -rf \"$BUNDLE\" 2>/dev/null; then\n        \
                     echo \"preinstall: removed existing $BUNDLE\"\n    \
                 else\n        \
                     owner=$(stat -f '%Su' \"$BUNDLE\" 2>/dev/null || echo unknown)\n        \
                     echo \"\" >&2\n        \
                     echo \"ERROR: Cannot remove $BUNDLE (owner: $owner).\" >&2\n        \
                     echo \"Either re-run with 'Install for all users of this computer',\" >&2\n        \
                     echo \"or run: sudo rm -rf \\\"$BUNDLE\\\"\" >&2\n        \
                     exit 1\n    \
                 fi\n\
             fi\n\
             exit 0\n",
        ),
    )?;
    Command::new("chmod")
        .args(["+x", preinstall.to_str().unwrap()])
        .status()?;

    Ok(scripts_dir)
}
