//! `Contents/Resources/moduleinfo.json` (VST3 SDK 3.7.5+), which lets a
//! host list a plug-in without loading it. Like the SDK's `moduleinfotool`,
//! it is read from the built module's own factory, so it can't drift from
//! what the binary reports. Only a host-arch binary can be loaded; cross
//! builds ship without it (the file is optional).

#![allow(clippy::cast_sign_loss, clippy::cast_possible_truncation, clippy::unnecessary_cast, clippy::needless_borrows_for_generic_args)]

use std::ffi::c_char;
use std::fmt::Write as _;
use std::path::Path;

use vst3::ComPtr;
use vst3::Steinberg::{
    IPluginFactory, IPluginFactory2, IPluginFactory2Trait, IPluginFactoryTrait, PClassInfo2,
    PFactoryInfo, PFactoryInfo_::FactoryFlags_,
};

/// Bundle-relative location.
pub(crate) const PATH: &str = "Contents/Resources/moduleinfo.json";

/// The JSON for `binary`, or `None` (with a log line) when it is not for
/// this host or its factory can't be read. Never fails the build.
pub(crate) fn generate(binary: &Path, module_name: &str, host_arch: bool) -> Option<String> {
    if !host_arch {
        crate::log_output(format!("VST3: no moduleinfo.json for a cross build of {module_name}"));
        return None;
    }
    // SAFETY: loads the plug-in we just built and calls its exported
    // VST3 entry point, as any host would.
    match unsafe { read_factory(binary, module_name) } {
        Ok(json) => Some(json),
        Err(e) => {
            crate::log_output(format!("VST3: moduleinfo.json skipped for {module_name}: {e}"));
            None
        }
    }
}

/// Write `json` into `bundle` (a writable bundle directory).
pub(crate) fn write(bundle: &Path, json: &str) -> crate::Res {
    let path = bundle.join(PATH);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, json)?;
    Ok(())
}

unsafe fn read_factory(binary: &Path, module_name: &str) -> Result<String, String> {
    let lib = unsafe { libloading::Library::new(binary) }.map_err(|e| e.to_string())?;
    let get = unsafe { lib.get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory") }
        .map_err(|e| e.to_string())?;
    let factory = unsafe { ComPtr::from_raw(get()) }.ok_or("GetPluginFactory returned null")?;
    let factory2 = factory.cast::<IPluginFactory2>().ok_or("no IPluginFactory2")?;

    let mut info: PFactoryInfo = unsafe { std::mem::zeroed() };
    unsafe { factory.getFactoryInfo(&raw mut info) };
    let mut classes = Vec::new();
    for i in 0..unsafe { factory.countClasses() } {
        let mut c: PClassInfo2 = unsafe { std::mem::zeroed() };
        if unsafe { factory2.getClassInfo2(i, &raw mut c) } == vst3::Steinberg::kResultOk {
            classes.push(c);
        }
    }
    drop((factory2, factory));
    // Unloading a Rust cdylib with live thread-locals can crash; this
    // process exits soon anyway.
    std::mem::forget(lib);
    Ok(render(module_name, &info, &classes))
}

fn render(module_name: &str, info: &PFactoryInfo, classes: &[PClassInfo2]) -> String {
    let flag = |bit: u32| info.flags as u32 & bit != 0;
    let mut out = String::new();
    let _ = write!(
        out,
        "{{\n  \"Name\": {},\n  \"Version\": {},\n  \"Factory Info\": {{\n    \"Vendor\": {},\n    \"URL\": {},\n    \"E-Mail\": {},\n    \"Flags\": {{\n      \"Unicode\": {},\n      \"Classes Discardable\": {},\n      \"Component Non Discardable\": {}\n    }}\n  }},\n  \"Classes\": [",
        quote(module_name),
        quote(classes.first().map_or_else(String::new, |c| text(&c.version))),
        quote(&text(&info.vendor)),
        quote(&text(&info.url)),
        quote(&text(&info.email)),
        flag(FactoryFlags_::kUnicode as u32),
        flag(FactoryFlags_::kClassesDiscardable as u32),
        flag(FactoryFlags_::kComponentNonDiscardable as u32),
    );
    for (i, c) in classes.iter().enumerate() {
        let subs = text(&c.subCategories);
        let subs: Vec<String> = subs.split('|').filter(|s| !s.is_empty()).map(quote).collect();
        let _ = write!(
            out,
            "{}\n    {{\n      \"CID\": {},\n      \"Category\": {},\n      \"Name\": {},\n      \"Vendor\": {},\n      \"Version\": {},\n      \"SDKVersion\": {},\n      \"Sub Categories\": [{}],\n      \"Class Flags\": {},\n      \"Cardinality\": {},\n      \"Snapshots\": []\n    }}",
            if i == 0 { "" } else { "," },
            quote(&cid_string(&c.cid.map(|b| b as u8), cfg!(windows))),
            quote(&text(&c.category)),
            quote(&text(&c.name)),
            quote(&text(&c.vendor)),
            quote(&text(&c.version)),
            quote(&text(&c.sdkVersion)),
            subs.join(", "),
            c.classFlags,
            c.cardinality,
        );
    }
    out.push_str("\n  ]\n}\n");
    out
}

/// `FUID::toString`: the 16 bytes in order, except that COM-compatible
/// builds (Windows) print the GUID's first three fields as integers.
fn cid_string(cid: &[u8; 16], com_compatible: bool) -> String {
    const COM_ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut s = String::with_capacity(32);
    for i in 0..16 {
        let _ = write!(s, "{:02X}", cid[if com_compatible { COM_ORDER[i] } else { i }]);
    }
    s
}

fn text(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn quote(s: impl AsRef<str>) -> String {
    let mut out = String::from("\"");
    for ch in s.as_ref().chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cid_matches_fuid_to_string() {
        let cid: [u8; 16] = std::array::from_fn(|i| i as u8);
        assert_eq!(cid_string(&cid, false), "000102030405060708090A0B0C0D0E0F");
        assert_eq!(cid_string(&cid, true), "030201000504070608090A0B0C0D0E0F");
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(quote("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }
}
