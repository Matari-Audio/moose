//! AAX build & install driver (`cargo moose build --aax` / `cargo moose install --aax`).
//!
//! Statically links each plugin's Rust `staticlib` (`lib<stem>_aax.a` /
//! `<stem>_aax.lib`) directly into the C++ `AAX_CMonolithicParameters` bundle
//! using `CMake` and the Avid AAX SDK (`AAX_SDK_PATH`). Zero `dlopen`.

#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::{Config, PluginDef, Res};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::path::Path;
use std::path::PathBuf;

/// Resolve the Avid AAX SDK root from `AAX_SDK_PATH` (env or `.cargo/config.toml`).
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub(crate) fn resolve_aax_sdk_path() -> Option<PathBuf> {
    let raw = crate::read_build_env("AAX_SDK_PATH")?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    path.is_dir().then_some(path)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn write_aax_template_sources(src_dir: &Path) -> Res {
    use crate::templates::aax;
    use crate::util::fs_ctx;

    fs_ctx::create_dir_all(src_dir)?;
    let files: &[(&str, &str)] = &[
        ("aax_shim_types.h", aax::SHIM_TYPES_H),
        ("Info.plist.in", aax::INFO_PLIST_IN),
        ("MooseAAX_Bridge.h", aax::BRIDGE_H),
        ("MooseAAX_Describe.cpp", aax::DESCRIBE_CPP),
        ("MooseAAX_Parameters.h", aax::PARAMETERS_H),
        ("MooseAAX_Parameters.cpp", aax::PARAMETERS_CPP),
        ("MooseAAX_GUI.h", aax::GUI_H),
        ("MooseAAX_GUI.cpp", aax::GUI_CPP),
    ];
    for &(name, content) in files {
        let dst = src_dir.join(name);
        // Only rewrite when content changed so CMake incremental builds stay warm.
        let needs_write = std::fs::read_to_string(&dst).ok().as_deref() != Some(content);
        if needs_write {
            fs_ctx::write(&dst, content)?;
        }
    }
    Ok(())
}

/// Build and stage one plugin's `.aaxplugin` bundle into `stage_dir`.
///
/// Returns `Ok(true)` when the bundle was staged, or `Ok(false)` when AAX was
/// skipped cleanly (unsupported OS or missing `AAX_SDK_PATH`).
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn emit_aax_bundle(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    stage_dir: &Path,
    target: Option<&str>,
    mac_slices: &[&str],
) -> Result<bool, crate::CargoMooseError> {
    use crate::templates::aax;
    use crate::util::fs_ctx;
    use std::process::Command;

    let Some(sdk_path) = resolve_aax_sdk_path() else {
        crate::log_skip(format!(
            "AAX ({}): skipped - set AAX_SDK_PATH to the Avid AAX SDK directory to build .aaxplugin bundles.",
            p.name
        ));
        return Ok(false);
    };

    let staticlib =
        crate::release_static_for_target(root, &format!("{}_aax", p.dylib_stem()), target);
    if !staticlib.exists() {
        return Err(format!(
            "AAX static link needs a Rust staticlib at {}\n\
             Ensure the plugin crate's Cargo.toml includes \"staticlib\" in crate-type:\n\
               crate-type = [\"cdylib\", \"staticlib\", \"rlib\"]",
            staticlib.display()
        )
        .into());
    }

    let sdk_path = sdk_path.canonicalize()?;
    #[cfg(target_os = "macos")]
    let arch = if mac_slices.is_empty() {
        match target.unwrap_or_else(|| moose_build::host_triple()) {
            "aarch64-apple-darwin" => "arm64".to_owned(),
            "x86_64-apple-darwin" => "x86_64".to_owned(),
            other => return Err(format!("unsupported macOS AAX target {other}").into()),
        }
    } else {
        mac_slices.join(";")
    };
    #[cfg(target_os = "windows")]
    let (arch, cmake_arch) = {
        let _ = mac_slices;
        match target.unwrap_or_else(|| moose_build::host_triple()) {
            "x86_64-pc-windows-msvc" => ("x64", "x64"),
            "aarch64-pc-windows-msvc" => ("arm64", "ARM64"),
            other => return Err(format!("unsupported Windows AAX target {other}").into()),
        }
    };
    let dynamic_crt = cfg!(target_os = "windows") && crate::util::msvc_dynamic_crt();
    let crt = if dynamic_crt { "MD" } else { "MT" };

    let target_dir = moose_build::target_dir(root);
    let proj_dir = target_dir.join("aax-cmake").join(&p.crate_name);
    let src_dir = proj_dir.join("src");
    let build_dir = proj_dir.join(format!("build-{arch}-{crt}"));
    fs_ctx::create_dir_all(&build_dir)?;

    for (name, source) in [
        ("CMakeLists.txt", aax::CMAKE_LISTS),
        ("AAXSDK.cmake", aax::SDK_CMAKE),
    ] {
        fs_ctx::write_if_changed(proj_dir.join(name), source)?;
    }
    write_aax_template_sources(&src_dir)?;

    let file_stem = p.file_stem();
    let bundle_id = format!("{}.{}.aax", config.vendor.id, p.bundle_id);
    let version = p.version.as_deref().unwrap_or("1.0.0");

    let mut configure = Command::new("cmake");
    configure
        .arg("-S")
        .arg(&proj_dir)
        .arg("-B")
        .arg(&build_dir)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg(format!("-DAAX_SDK_PATH={}", sdk_path.display()))
        .arg(format!(
            "-DMOOSE_AAX_SDK_CACHE={}",
            target_dir.join("aax-sdk-cache").display()
        ))
        .arg(format!("-DMOOSE_RUST_STATICLIB={}", staticlib.display()))
        .arg(format!("-DMOOSE_PLUGIN_STEM={file_stem}"))
        .arg(format!("-DMOOSE_BUNDLE_ID={bundle_id}"))
        .arg(format!("-DMOOSE_PLUGIN_VERSION={version}"));
    if let Some(archive) = crate::read_build_env("AAX_LIB_PATH") {
        configure.arg(format!("-DAAX_LIB_PATH={archive}"));
    } else {
        configure.arg("-UAAX_LIB_PATH");
    }
    #[cfg(target_os = "macos")]
    configure
        .arg(format!("-DCMAKE_OSX_ARCHITECTURES={arch}"))
        .arg(format!(
            "-DCMAKE_OSX_DEPLOYMENT_TARGET={}",
            crate::deployment_target()
        ));
    #[cfg(target_os = "windows")]
    {
        configure
            .arg(format!("-DMOOSE_AAX_WINDOWS_ARCH={arch}"))
            .arg(format!("-DMOOSE_MSVC_CRT={crt}"));
        if std::env::var("CMAKE_GENERATOR").map_or(true, |name| name.starts_with("Visual Studio")) {
            configure.args(["-A", cmake_arch]);
        }
    }
    let status = configure
        .status()
        .map_err(|e| format!("running `cmake` to configure AAX bundle: {e}"))?;
    if !status.success() {
        return Err(format!("AAX cmake configure failed for {}", p.name).into());
    }

    let build_status = Command::new("cmake")
        .args(["--build"])
        .arg(&build_dir)
        .args(["--config", "Release"])
        .status()
        .map_err(|e| format!("running `cmake --build` for AAX bundle: {e}"))?;
    if !build_status.success() {
        return Err(format!("AAX cmake build failed for {}", p.name).into());
    }

    let bundle_name = format!("{file_stem}.aaxplugin");
    let dst_bundle = stage_dir.join(&bundle_name);
    let _ = std::fs::remove_dir_all(&dst_bundle);

    #[cfg(target_os = "macos")]
    {
        let built_bundle = [
            build_dir.join("Release").join(&bundle_name),
            build_dir.join(&bundle_name),
        ]
        .into_iter()
        .find(|candidate| candidate.is_dir())
        .ok_or_else(|| format!("AAX build did not produce {bundle_name}"))?;
        crate::util::copy_dir_recursive(&built_bundle, &dst_bundle)?;
        crate::codesign_bundle(
            dst_bundle.to_str().unwrap_or_default(),
            &crate::application_identity(),
            false,
        )?;
    }

    #[cfg(target_os = "windows")]
    {
        let built_dll = [
            build_dir.join("Release").join(&bundle_name),
            build_dir.join(&bundle_name),
        ]
        .into_iter()
        .find(|cand| cand.exists())
        .ok_or_else(|| format!("AAX build did not produce {bundle_name}"))?;
        let arch_dir = dst_bundle.join("Contents").join(arch);
        fs_ctx::create_dir_all(&arch_dir)?;
        fs_ctx::copy(&built_dll, arch_dir.join(&bundle_name))?;
    }

    Ok(true)
}

#[cfg(target_os = "macos")]
pub(crate) fn avid_root() -> PathBuf {
    PathBuf::from("/Library/Application Support/Avid")
}

#[cfg(target_os = "windows")]
pub(crate) fn avid_root() -> PathBuf {
    let base = std::env::var_os("CommonProgramFiles").map_or_else(
        || PathBuf::from(r"C:\Program Files\Common Files"),
        PathBuf::from,
    );
    base.join("Avid")
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn aax_plugins_dir() -> PathBuf {
    avid_root().join("Audio").join("Plug-Ins")
}

/// Install a pre-built `.aaxplugin` bundle from `target/bundles/` into the
/// Avid system plug-ins directory.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn install_aax(root: &Path, p: &PluginDef, _config: &Config) -> Res {
    let bundle_name = format!("{}.aaxplugin", p.file_stem());
    let built = moose_build::target_dir(root)
        .join("bundles")
        .join(&bundle_name);
    if !built.exists() {
        if resolve_aax_sdk_path().is_none() {
            return Ok(());
        }
        return Err(format!(
            "AAX: bundle missing for {} at {}. Run `cargo moose build --aax -p {}` to produce it.",
            p.name,
            built.display(),
            p.crate_name
        )
        .into());
    }

    if !avid_root().exists() {
        crate::log_skip(format!(
            "AAX: skipped {} - Pro Tools / Media Composer not detected at {}.",
            p.name,
            avid_root().display()
        ));
        return Ok(());
    }

    let aax_dir = aax_plugins_dir();
    let dst = aax_dir.join(&bundle_name);

    #[cfg(target_os = "macos")]
    {
        use std::ffi::OsStr;
        crate::run_sudo("rm", &[OsStr::new("-rf"), dst.as_os_str()])?;
        crate::run_sudo("ditto", &[built.as_os_str(), dst.as_os_str()])?;
    }

    #[cfg(target_os = "windows")]
    {
        crate::util::fs_ctx::create_dir_all(&aax_dir)?;
        let _ = std::fs::remove_dir_all(&dst);
        crate::util::copy_dir_recursive(&built, &dst)?;
    }

    crate::log_output(format!("AAX:  {}", dst.display()));
    Ok(())
}
