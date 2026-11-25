//! Factory presets (`kAudioUnitProperty_FactoryPresets`).

use crate::callbacks::load_custom_state;
use crate::instance::AuInstance;

use moose_core::cast::len_u32;
use moose_core::export::PluginExport;
use moose_core::presets::{PresetScope, enumerate_scope};
use moose_core::state;
use moose_core::wrapper::run_extern_callback_with;
use std::ffi::CString;
use std::os::raw::c_char;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Factory presets (kAudioUnitProperty_FactoryPresets backing)
// ---------------------------------------------------------------------------

/// One bundled factory preset: the display name handed to the shim
/// (C-string, lives for the process) plus the file to load.
pub(crate) struct FactoryPresetEntry {
    pub(crate) name: CString,
    pub(crate) path: std::path::PathBuf,
}

/// Lazily-enumerated factory presets from the component bundle's
/// `Contents/Resources/Presets/`. One static per shared library, like
/// the registration statics: an AU dylib ships exactly one plugin
/// type, so the single `OnceLock` never sees a second
/// monomorphization. Empty when the bundle ships no presets (or when
/// the dylib isn't inside a component bundle, e.g. the AU v3 appex
/// layout - the shim then reports the property as invalid).
pub(crate) static FACTORY_PRESETS: OnceLock<Vec<FactoryPresetEntry>> = OnceLock::new();

pub(crate) fn factory_presets<P: PluginExport>() -> &'static [FactoryPresetEntry] {
    FACTORY_PRESETS.get_or_init(|| {
        let Some(root) = component_presets_root::<P>() else {
            return Vec::new();
        };
        let info = P::info();
        let mut refs = enumerate_scope(
            &root,
            PresetScope::Factory,
            info.vendor,
            info.name,
            state::shared_plugin_state_hash(&info),
            info.preset_extension,
        );
        // The library's `default = true` preset leads the list: hosts
        // treat factory preset 0 as the de-facto initial sound. The
        // stable sort keeps the walk's alphabetical order behind it.
        refs.sort_by_key(|preset| !preset.default);
        refs.into_iter()
            .filter_map(|preset| {
                // Hosts show the factory list flat; keep the category
                // visible the way the LV2 labels do.
                let display = match &preset.category {
                    Some(category) => format!("{category}/{}", preset.name),
                    None => preset.name.clone(),
                };
                Some(FactoryPresetEntry {
                    name: CString::new(display).ok()?,
                    path: preset.path,
                })
            })
            .collect()
    })
}

/// Mirrors the layout `<libc/dlfcn.h>` defines; bound directly like
/// the `malloc` / `free` externs above to keep the crate free of a
/// libc dependency. Field names keep dlfcn's `dli_` prefix so they
/// line up with the C declaration they shadow.
#[cfg(unix)]
#[repr(C)]
#[allow(clippy::struct_field_names)]
pub(crate) struct DlInfo {
    pub(crate) dli_fname: *const c_char,
    pub(crate) dli_fbase: *mut std::ffi::c_void,
    pub(crate) dli_sname: *const c_char,
    pub(crate) dli_saddr: *mut std::ffi::c_void,
}

#[cfg(unix)]
unsafe extern "C" {
    pub(crate) fn dladdr(addr: *const std::ffi::c_void, info: *mut DlInfo) -> i32;
}

/// Resolve the factory-presets directory of the bundle this code lives
/// in, via `dladdr` on one of our own functions. Three layouts exist:
///
/// - AU v2 component (macOS): `<X>.component/Contents/MacOS/<X>` with
///   presets in `Contents/Resources/Presets/` (two levels up).
/// - AU v3 framework (macOS): `<F>.framework/Versions/A/<F>` with presets
///   in `Versions/A/Resources/Presets/` (one level up).
/// - AU v3 framework (iOS): `<F>.framework/<F>` - a shallow bundle, so
///   presets live in a flat `<F>.framework/Presets/` (one level up; a
///   `Resources/` subdir would make iOS installd reject the framework).
#[cfg(unix)]
pub(crate) fn component_presets_root<P: PluginExport>() -> Option<std::path::PathBuf> {
    let mut info = DlInfo {
        dli_fname: std::ptr::null(),
        dli_fbase: std::ptr::null_mut(),
        dli_sname: std::ptr::null(),
        dli_saddr: std::ptr::null_mut(),
    };
    let probe = cb_factory_preset_count::<P> as *const std::ffi::c_void;
    // SAFETY: `probe` is a function in this image; `dladdr` only
    // writes into the out-struct on success.
    if unsafe { dladdr(probe, &raw mut info) } == 0 || info.dli_fname.is_null() {
        return None;
    }
    // SAFETY: `dli_fname` is a NUL-terminated path owned by dyld.
    let exe = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) };
    let exe = std::path::Path::new(exe.to_str().ok()?);
    let parent = exe.parent()?;
    // Probe order: macOS `Resources/Presets` two levels up (AU v2
    // component) and one level up (AU v3 framework), then the iOS flat
    // `Presets/` one level up.
    [
        parent.parent().map(|d| d.join("Resources/Presets")),
        Some(parent.join("Resources/Presets")),
        Some(parent.join("Presets")),
    ]
    .into_iter()
    .flatten()
    .find(|root| root.is_dir())
}

#[cfg(not(unix))]
#[allow(
    clippy::extra_unused_type_parameters,
    reason = "matches the Unix implementation, which uses P to locate the plugin image"
)]
pub(crate) fn component_presets_root<P: PluginExport>() -> Option<std::path::PathBuf> {
    None
}

pub(crate) unsafe extern "C" fn cb_factory_preset_count<P: PluginExport>(
    _ctx: *mut std::ffi::c_void,
) -> u32 {
    run_extern_callback_with::<P, u32>("au", "factory_preset_count", 0, || {
        len_u32(factory_presets::<P>().len())
    })
}

pub(crate) unsafe extern "C" fn cb_factory_preset_name<P: PluginExport>(
    _ctx: *mut std::ffi::c_void,
    index: u32,
) -> *const c_char {
    run_extern_callback_with::<P, *const c_char>(
        "au",
        "factory_preset_name",
        std::ptr::null(),
        || {
            factory_presets::<P>()
                .get(index as usize)
                .map_or(std::ptr::null(), |entry| entry.name.as_ptr())
        },
    )
}

pub(crate) unsafe extern "C" fn cb_factory_preset_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
) -> i32 {
    // SAFETY: The shim calls `factory_preset_load` only with its live instance (`au_v2_shim.c`
    // checks `rustCtx`); only a shared reference is formed.
    run_extern_callback_with::<P, i32>("au", "factory_preset_load", 0, || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let Some(entry) = factory_presets::<P>().get(index as usize) else {
            return 0;
        };
        // Migration-aware read: the plugin's `StateMigrationChain` runs and
        // v2 string IDs resolve before the params are touched.
        let Some(deserialized) =
            state::load_preset_file::<P>(&entry.path, inst.plugin_id_hash, &inst.params_arc)
        else {
            return 0;
        };
        // Same apply path as cb_state_load: params synchronously on
        // the host thread, full state through the audio-thread
        // handoff, editor notified.
        let backup = state::ParamsBackup::capture(&*inst.params_arc);
        state::apply_params(&*inst.params_arc, &deserialized);
        if !load_custom_state(inst, deserialized, &backup) {
            return 0;
        }
        if let Some(ref mut editor) = inst.gui.enter().editor {
            editor.state_changed();
        }
        1
    })
}
