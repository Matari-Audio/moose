//! `Contents/Resources/moduleinfo.json` (VST3 SDK 3.7.5+), which lets a
//! host list a plug-in without loading it. Like the SDK's `moduleinfotool`,
//! it is read from the built module's own factory, so it can't drift from
//! what the binary reports. Only a host-arch binary can be loaded; cross
//! builds ship without it (the file is optional).

#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::unnecessary_cast,
    clippy::needless_borrows_for_generic_args
)]

use std::ffi::{c_char, c_void};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;

use vst3::Steinberg::{
    IBStream, IBStreamTrait, IPluginCompatibility, IPluginCompatibility_iid,
    IPluginCompatibilityTrait, IPluginFactory, IPluginFactory2, IPluginFactory2Trait,
    IPluginFactoryTrait, PClassInfo2, PFactoryInfo, PFactoryInfo_::FactoryFlags_, int32, int64,
    kInvalidArgument, kNotImplemented, kResultOk, tresult,
};
use vst3::{Class, ComPtr, ComWrapper};

const COMPATIBILITY_CATEGORY: &str = "Plugin Compatibility Class";

struct JsonCaptureStream {
    bytes: Mutex<Vec<u8>>,
}

impl Class for JsonCaptureStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for JsonCaptureStream {
    unsafe fn read(&self, _buffer: *mut c_void, _num_bytes: int32, _read: *mut int32) -> tresult {
        kNotImplemented
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        num_bytes: int32,
        num_bytes_written: *mut int32,
    ) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            return kInvalidArgument;
        }
        // SAFETY: `buffer` is non-null and `num_bytes >= 0` (checked above), and the plugin passes
        // `num_bytes` readable bytes per `IBStream`.
        let slice = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), num_bytes as usize) };
        self.bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(slice);
        if !num_bytes_written.is_null() {
            // SAFETY: `num_bytes_written` is non-null (checked above) and writable per `IBStream`.
            unsafe { *num_bytes_written = num_bytes };
        }
        kResultOk
    }

    unsafe fn seek(&self, _pos: int64, _mode: int32, _result: *mut int64) -> tresult {
        kNotImplemented
    }

    unsafe fn tell(&self, _pos: *mut int64) -> tresult {
        kNotImplemented
    }
}

/// Bundle-relative location.
pub(crate) const PATH: &str = "Contents/Resources/moduleinfo.json";

/// The JSON for `binary`, or `None` (with a log line) when it is not for
/// this host or its factory can't be read. Never fails the build.
pub(crate) fn generate(binary: &Path, module_name: &str, host_arch: bool) -> Option<String> {
    if !host_arch {
        crate::log_output(format!(
            "VST3: no moduleinfo.json for a cross build of {module_name}"
        ));
        return None;
    }
    // SAFETY: loads the plug-in we just built and calls its exported
    // VST3 entry point, as any host would.
    match unsafe { read_factory(binary, module_name) } {
        Ok(json) => Some(json),
        Err(e) => {
            crate::log_output(format!(
                "VST3: moduleinfo.json skipped for {module_name}: {e}"
            ));
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
    // SAFETY: `binary` is a moose-built VST3 module, the caller's contract for `read_factory`;
    // loading it runs only moose's own initialisers.
    let lib = unsafe { libloading::Library::new(binary) }.map_err(|e| e.to_string())?;
    // SAFETY: `GetPluginFactory` has this signature in every VST3 module.
    let get = unsafe {
        lib.get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory")
    }
    .map_err(|e| e.to_string())?;
    // SAFETY: `GetPluginFactory` returns null or an owned factory reference, which `from_raw`
    // adopts. `lib` outlives `factory` (declared first, dropped last).
    let factory = unsafe { ComPtr::from_raw(get()) }.ok_or("GetPluginFactory returned null")?;
    let factory2 = factory
        .cast::<IPluginFactory2>()
        .ok_or("no IPluginFactory2")?;

    // SAFETY: `PFactoryInfo` is plain integers and char arrays, so all-zero is a valid value.
    let mut info: PFactoryInfo = unsafe { std::mem::zeroed() };
    // SAFETY: `factory` is live, and `info` is a writable local.
    unsafe { factory.getFactoryInfo(&raw mut info) };
    let mut classes = Vec::new();
    let mut compatibility: Option<String> = None;
    // SAFETY: `factory` is live, and `countClasses` takes no pointers.
    for i in 0..unsafe { factory.countClasses() } {
        // SAFETY: `PClassInfo2` is plain integers and char arrays, so all-zero is a valid value.
        let mut c: PClassInfo2 = unsafe { std::mem::zeroed() };
        // SAFETY: `factory2` is live, and `c` is a writable local.
        if unsafe { factory2.getClassInfo2(i, &raw mut c) } == kResultOk {
            if compatibility.is_none() && text(&c.category) == COMPATIBILITY_CATEGORY {
                // SAFETY: `factory` is live and `c.cid` is a 16-byte class id from that factory.
                compatibility = unsafe { read_compatibility_json(&factory, &c.cid) };
            }
            classes.push(c);
        }
    }
    drop((factory2, factory));
    // Unloading a Rust cdylib with live thread-locals can crash; this
    // process exits soon anyway.
    std::mem::forget(lib);
    Ok(render(
        module_name,
        &info,
        &classes,
        compatibility.as_deref(),
    ))
}

unsafe fn read_compatibility_json(
    factory: &ComPtr<IPluginFactory>,
    cid: &[c_char; 16],
) -> Option<String> {
    let mut obj = std::ptr::null_mut();
    // SAFETY: `factory` is live, `cid` and the IID are 16-byte ids, and `obj` is a writable local.
    let rc = unsafe {
        factory.createInstance(
            cid.as_ptr(),
            IPluginCompatibility_iid.as_ptr().cast(),
            &raw mut obj,
        )
    };
    if rc != kResultOk || obj.is_null() {
        return None;
    }
    // SAFETY: `createInstance` succeeded with a non-null `obj`, an owned `IPluginCompatibility`
    // reference that `from_raw` adopts.
    let compat = unsafe { ComPtr::from_raw(obj.cast::<IPluginCompatibility>()) }?;
    let stream = ComWrapper::new(JsonCaptureStream {
        bytes: Mutex::new(Vec::new()),
    });
    let stream_ptr = stream.to_com_ptr::<IBStream>()?;
    // SAFETY: `compat` and `stream_ptr` are live for the call.
    if unsafe { compat.getCompatibilityJSON(stream_ptr.as_ptr()) } != kResultOk {
        return None;
    }
    let bytes = stream
        .bytes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let raw = String::from_utf8(bytes).ok()?;
    normalize_compatibility_json(&raw)
}

/// Normalize a VST3 `IPluginCompatibility::getCompatibilityJSON` payload (which
/// may use JSON5 comments inside or outside the root array and trailing commas)
/// into a standard JSON array suitable for embedding in `moduleinfo.json`.
fn normalize_compatibility_json(raw: &str) -> Option<String> {
    let cleaned = strip_json5_comments_and_trailing_commas(raw);
    let trimmed = cleaned.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return None;
    }
    // Verify the opening `[` matches the final `]` (no trailing garbage or
    // concatenated root values outside strings).
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut closed_at = None;
    for (idx, ch) in trimmed.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '[' | '{' => depth += 1,
            ']' | '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    closed_at = Some(idx);
                    break;
                }
            }
            _ => {}
        }
    }
    if in_string || depth != 0 || closed_at != Some(trimmed.len() - 1) {
        return None;
    }
    Some(trimmed.to_owned())
}

fn strip_json5_comments_and_trailing_commas(input: &str) -> String {
    let mut chars = input.chars().peekable();
    let mut no_comments = String::with_capacity(input.len());
    let mut in_string = None;
    while let Some(ch) = chars.next() {
        if let Some(quote) = in_string {
            if ch == '\\'
                && let Some(escaped) = chars.next()
            {
                match escaped {
                    '\r' => {
                        chars.next_if_eq(&'\n');
                    }
                    '\n' | '\u{2028}' | '\u{2029}' => {}
                    _ => {
                        no_comments.push(ch);
                        no_comments.push(escaped);
                    }
                }
                continue;
            }
            if ch == quote {
                in_string = None;
            }
        } else if matches!(ch, '"' | '\'') {
            in_string = Some(ch);
        } else if ch == '/' && chars.next_if_eq(&'/').is_some() {
            while chars
                .next_if(|c| !matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
                .is_some()
            {}
            continue;
        } else if ch == '/' && chars.next_if_eq(&'*').is_some() {
            while let Some(c) = chars.next() {
                if c == '*' && chars.next_if_eq(&'/').is_some() {
                    break;
                }
            }
            continue;
        } else if matches!(ch, '\u{2028}' | '\u{2029}') {
            no_comments.push('\n');
            continue;
        }
        no_comments.push(ch);
    }

    // Strip trailing commas immediately preceding `]` or `}` outside strings.
    let chars: Vec<char> = no_comments.chars().collect();
    let mut out = String::with_capacity(no_comments.len());
    let mut j = 0;
    let mut str_quote: Option<char> = None;
    while j < chars.len() {
        let ch = chars[j];
        if let Some(q) = str_quote {
            if ch == '\\' && j + 1 < chars.len() {
                out.push(ch);
                out.push(chars[j + 1]);
                j += 2;
                continue;
            }
            if ch == q {
                str_quote = None;
            }
            out.push(ch);
            j += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            str_quote = Some(ch);
            out.push(ch);
            j += 1;
            continue;
        }
        if ch == ',' {
            let mut look = j + 1;
            while look < chars.len() && chars[look].is_whitespace() {
                look += 1;
            }
            if look < chars.len() && (chars[look] == ']' || chars[look] == '}') {
                j += 1;
                continue;
            }
        }
        out.push(ch);
        j += 1;
    }
    out
}

fn render(
    module_name: &str,
    info: &PFactoryInfo,
    classes: &[PClassInfo2],
    compatibility: Option<&str>,
) -> String {
    let flag = |bit: u32| info.flags as u32 & bit != 0;
    let mut out = String::new();
    let _ = write!(
        out,
        "{{\n  \"Name\": {},\n  \"Version\": {},\n  \"Factory Info\": {{\n    \"Vendor\": {},\n    \"URL\": {},\n    \"E-Mail\": {},\n    \"Flags\": {{\n      \"Unicode\": {},\n      \"Classes Discardable\": {},\n      \"Component Non Discardable\": {}\n    }}\n  }},\n  \"Classes\": [",
        quote(module_name),
        quote(
            classes
                .first()
                .map_or_else(String::new, |c| text(&c.version))
        ),
        quote(&text(&info.vendor)),
        quote(&text(&info.url)),
        quote(&text(&info.email)),
        flag(FactoryFlags_::kUnicode as u32),
        flag(FactoryFlags_::kClassesDiscardable as u32),
        flag(FactoryFlags_::kComponentNonDiscardable as u32),
    );
    for (i, c) in classes.iter().enumerate() {
        let subs = text(&c.subCategories);
        let subs: Vec<String> = subs
            .split('|')
            .filter(|s| !s.is_empty())
            .map(quote)
            .collect();
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
    if let Some(compat) = compatibility {
        let _ = write!(out, "\n  ],\n  \"Compatibility\": {compat}\n}}\n");
    } else {
        out.push_str("\n  ]\n}\n");
    }
    out
}

/// `FUID::toString`: the 16 bytes in order, except that COM-compatible
/// builds (Windows) print the GUID's first three fields as integers.
fn cid_string(cid: &[u8; 16], com_compatible: bool) -> String {
    const COM_ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut s = String::with_capacity(32);
    for i in 0..16 {
        let _ = write!(
            s,
            "{:02X}",
            cid[if com_compatible { COM_ORDER[i] } else { i }]
        );
    }
    s
}

fn text(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
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

    #[test]
    fn compatibility_preserves_mappings_and_decoded_strings() {
        for newline in ["\n", "\r", "\r\n", "\u{2028}", "\u{2029}"] {
            let half = "A".repeat(16);
            let raw = format!(
                "[// aliases{newline}{{\"New\":\"{half}\\{newline}{half}\",\"Name\":\"日本語\"}}]"
            );
            let normalized = normalize_compatibility_json(&raw).unwrap();
            let decoded: serde_json::Value = serde_json::from_str(&normalized).unwrap();
            assert_eq!(decoded[0]["New"], "A".repeat(32));
            assert_eq!(decoded[0]["Name"], "日本語");
        }
    }

    #[test]
    fn render_includes_compatibility_array_when_present() {
        // SAFETY: `PFactoryInfo` is plain integers and char arrays, so all-zero is a valid value.
        let info: PFactoryInfo = unsafe { std::mem::zeroed() };
        let compat = r#"[{"New":"00112233445566778899AABBCCDDEEFF","Old":["FFEEDDCCBBAA99887766554433221100"]}]"#;
        let with = render("Demo", &info, &[], Some(compat));
        assert!(with.contains(&format!("\"Compatibility\": {compat}")));
        let without = render("Demo", &info, &[], None);
        assert!(!without.contains("\"Compatibility\""));
    }

    #[test]
    fn normalize_compatibility_accepts_json5_comments_and_trailing_commas() {
        let payload = r#"
            /* leading comment */
            [
              // line comment inside array
              { "New": "00112233445566778899AABBCCDDEEFF", "Old": ["FFEEDDCCBBAA99887766554433221100",], },
            ]
            /* trailing compatibility comment */
        "#;
        let normalized = normalize_compatibility_json(payload).expect("valid JSON5 array");
        assert!(normalized.starts_with('[') && normalized.ends_with(']'));
        assert!(normalized.contains("00112233445566778899AABBCCDDEEFF"));
        assert!(!normalized.contains("leading comment"));
        assert!(!normalized.contains("trailing compatibility"));
        assert!(!normalized.contains(",]"));
        assert!(!normalized.contains(",}"));
        assert!(normalize_compatibility_json(r#"{"not":"an-array"}"#).is_none());
        assert!(normalize_compatibility_json(r"[] []").is_none());
    }
}
