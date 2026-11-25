//! VST3 COM objects on the official SDK bindings (`vst3` crate, generated
//! from the MIT `pluginterfaces` headers).
//!
//! The objects here own everything host-facing: interface layout, IIDs,
//! result codes, threading of host calls. Plugin behaviour lives directly
//! behind the typed [`crate::backend::Vst3Backend`] trait.
//!
//! The SDK's enum constants are `u32` on Unix and `i32` on Windows, and
//! its counts and indices are `int32`; the casts between them are
//! deliberate and range-checked where a host value feeds an index.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::unnecessary_cast,
    clippy::wildcard_imports
)]

mod component;
mod factory;
#[cfg(test)]
mod native_tests;
mod view;
mod wake;
#[cfg(target_os = "linux")]
mod wayland;

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use vst3::Steinberg::{IBStream, IBStreamTrait, kResultFalse, kResultOk};
use vst3::{ComPtr, ComRef};

use crate::backend::{ParamDescriptor, PluginDescriptor, Vst3FactoryBackend};

pub(crate) use component::Host;

/// Everything `register` hands over, plus the unit tree derived from it.
pub(crate) struct Registration {
    pub desc: PluginDescriptor,
    pub factory: Box<dyn Vst3FactoryBackend>,
    pub params: Vec<ParamDescriptor>,
    pub units: Vec<Unit>,
    /// `IUnitInfo` unit id per entry of `params`.
    pub param_unit: Vec<i32>,
}

pub(crate) struct Unit {
    pub id: i32,
    pub parent: i32,
    pub name: String,
}

static REGISTRATION: OnceLock<Registration> = OnceLock::new();

pub(crate) fn registration() -> Option<&'static Registration> {
    REGISTRATION.get()
}

pub(crate) fn register(
    desc: PluginDescriptor,
    factory: Box<dyn Vst3FactoryBackend>,
    params: Vec<ParamDescriptor>,
) {
    let (units, param_unit) = build_units(&params);
    let _ = REGISTRATION.set(Registration {
        desc,
        factory,
        params,
        units,
        param_unit,
    });
}

/// One unit per distinct `/`-separated group path, root unit 0.
pub(crate) fn build_units(params: &[ParamDescriptor]) -> (Vec<Unit>, Vec<i32>) {
    let mut units = vec![Unit {
        id: 0,
        parent: -1,
        name: "Root".into(),
    }];
    let mut paths: HashMap<String, i32> = HashMap::new();
    let param_unit = params
        .iter()
        .map(|p| {
            let mut parent = 0;
            let mut path = String::new();
            for name in p.group.split('/').filter(|s| !s.is_empty()) {
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(name);
                parent = *paths.entry(path.clone()).or_insert_with(|| {
                    let id = i32::try_from(units.len()).unwrap_or(i32::MAX);
                    units.push(Unit {
                        id,
                        parent,
                        name: name.into(),
                    });
                    id
                });
            }
            parent
        })
        .collect();
    (units, param_unit)
}

pub(crate) fn factory() -> *mut c_void {
    factory::new_factory()
}

// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

/// `&str` from a host C string; empty for null or invalid UTF-8.
pub(crate) unsafe fn cstr<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        ""
    } else {
        // SAFETY: `p` is non-null, and the caller guarantees it is a NUL-terminated host string
        // that outlives `'a`.
        unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
    }
}

/// UTF-16 copy into a fixed host buffer, NUL-terminated, never splitting a
/// surrogate pair.
pub(crate) fn utf16_into(dst: &mut [u16], s: &str) {
    let Some(max) = dst.len().checked_sub(1) else {
        return;
    };
    let mut n = 0;
    for ch in s.chars() {
        let mut buf = [0u16; 2];
        let units = ch.encode_utf16(&mut buf);
        if n + units.len() > max {
            break;
        }
        dst[n..n + units.len()].copy_from_slice(units);
        n += units.len();
    }
    dst[n] = 0;
}

/// UTF-8 copy into a fixed `char8` buffer, NUL-terminated, cut on a char
/// boundary.
pub(crate) fn c8_into(dst: &mut [c_char], s: &str) {
    let Some(max) = dst.len().checked_sub(1) else {
        return;
    };
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    for (d, b) in dst.iter_mut().zip(&s.as_bytes()[..end]) {
        *d = *b as c_char;
    }
    dst[end] = 0;
}

/// Host UTF-16 text (NUL-terminated) to a `String`.
pub(crate) unsafe fn utf16_from(p: *const u16, max: usize) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0;
    // SAFETY: `len < max`, and the caller guarantees `p` is readable up to its NUL or `max` units,
    // whichever comes first.
    while len < max && unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: The loop stopped at the NUL or at `max`, so all `len` units were just read and are in
    // bounds.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

// ---------------------------------------------------------------------------
// Host state streams
// ---------------------------------------------------------------------------

/// One bounded read from a host `IBStream` straight into the Rust parser.
/// Returns the byte count, or -1 on an error or an impossible count.
pub(crate) fn read_state_chunk(stream: ComRef<'_, IBStream>, dst: &mut [u8]) -> i64 {
    let Ok(requested) = u32::try_from(dst.len()) else {
        return -1;
    };
    if requested == 0 || requested > 8192 {
        return -1;
    }
    let mut count = 0;
    // SAFETY: `stream` is a live host `IBStream`; `dst` is writable for `requested` bytes and
    // `count` is a local out slot.
    let result = unsafe { stream.read(dst.as_mut_ptr().cast(), requested as i32, &raw mut count) };
    if count < 0 || count as u32 > requested || (result != kResultOk && result != kResultFalse) {
        return -1;
    }
    i64::from(count)
}

/// One bounded write to a host `IBStream`. Returns the byte count, or -1.
pub(crate) fn write_state_chunk(stream: ComRef<'_, IBStream>, src: &[u8]) -> i64 {
    let Ok(requested) = u32::try_from(src.len()) else {
        return -1;
    };
    if requested == 0 || requested > 8192 {
        return -1;
    }
    let mut count = 0;
    // SAFETY: `stream` is a live host `IBStream`; `src` is readable for `requested` bytes (the SDK
    // signature takes `*mut` but `write` only reads) and `count` is a local out slot.
    let result = unsafe {
        stream.write(
            src.as_ptr().cast_mut().cast(),
            requested as i32,
            &raw mut count,
        )
    };
    if count <= 0 || count as u32 > requested || result != kResultOk {
        return -1;
    }
    i64::from(count)
}

// ---------------------------------------------------------------------------
// Instance registry: monotonic instance ID -> live host object
// ---------------------------------------------------------------------------

static INSTANCES: Mutex<Vec<(usize, Weak<Host>)>> = Mutex::new(Vec::new());

fn instances() -> std::sync::MutexGuard<'static, Vec<(usize, Weak<Host>)>> {
    INSTANCES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn register_instance(host: &Arc<Host>) {
    instances().push((host.id, Arc::downgrade(host)));
}

pub(crate) fn unregister_instance(id: usize) {
    instances().retain(|(key, _)| *key != id);
}

/// The live instance for `id`, held for the duration of the call.
pub(crate) fn pin(id: usize) -> Option<Arc<Host>> {
    if id == 0 {
        return None;
    }
    instances()
        .iter()
        .find(|(key, _)| *key == id)
        .and_then(|(_, weak)| weak.upgrade())
}

// ---------------------------------------------------------------------------
// Host notifications from Vst3Instance / Vst3Host
// ---------------------------------------------------------------------------

pub(crate) fn begin_edit(host_id: usize, id: u32) {
    if let Some(host) = pin(host_id) {
        host.submit(component::EditOp::Begin(id));
    }
}

pub(crate) fn perform_edit(host_id: usize, id: u32, normalized: f64) {
    if let Some(host) = pin(host_id) {
        host.submit(component::EditOp::Perform(id, normalized));
    }
}

pub(crate) fn end_edit(host_id: usize, id: u32) {
    if let Some(host) = pin(host_id) {
        host.submit(component::EditOp::End(id));
    }
}

pub(crate) fn request_resize(host_id: usize, w: u32, h: u32) -> bool {
    pin(host_id).is_some_and(|host| host.submit(component::EditOp::Resize(w, h)) != 0)
}

pub(crate) fn mark_dirty(host_id: usize) {
    if let Some(host) = pin(host_id) {
        host.submit(component::EditOp::Dirty);
    }
}

pub(crate) fn group_edit(host_id: usize, edge: moose_core::editor::EditGroup) {
    if let Some(host) = pin(host_id) {
        host.submit(component::EditOp::Group(edge));
    }
}

pub(crate) fn mark_restart(host_id: usize, flags: i32) {
    component::mark_restart(host_id, flags);
}

pub(crate) fn flush_host_queue(host_id: usize) {
    component::flush_host_queue(host_id);
}

pub(crate) unsafe fn wake_host_queue(host: *const Host) {
    // SAFETY: Forwarded contract: the caller guarantees `host` is null or the opaque `HostWakes`
    // token from `Host::new`, which is `component::wake_host_queue`'s contract.
    unsafe { component::wake_host_queue(host) };
}

pub(crate) fn check_presentation(host_id: usize) {
    if let Some(host) = pin(host_id) {
        host.check_presentation();
    }
}

pub(crate) fn has_context_menu(host_id: usize) -> bool {
    pin(host_id).is_some_and(|host| host.has_context_menu())
}

pub(crate) fn popup_context_menu(
    host_id: usize,
    target: moose_core::main_thread::MenuTarget,
    x: i32,
    y: i32,
    items: &[moose_core::main_thread::MenuItem],
) {
    if let Some(host) = pin(host_id) {
        host.popup_context_menu(target, x, y, items);
    }
}

/// `ComPtr` from an incoming interface pointer, adding a reference.
pub(crate) unsafe fn retain<I: vst3::Interface>(p: *mut I) -> Option<ComPtr<I>> {
    // SAFETY: The caller guarantees `p` is null or a live interface pointer for the duration of
    // this call; `to_com_ptr` adds the reference we keep.
    unsafe { ComRef::from_raw(p) }.map(|r| r.to_com_ptr())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_decodes_and_never_splits_a_pair() {
        let mut b = [0u16; 8];
        utf16_into(&mut b, "\u{b5}");
        assert_eq!(&b[..2], &[0xB5, 0]);
        utf16_into(&mut b, "\u{1F600}");
        assert_eq!(&b[..3], &[0xD83D, 0xDE00, 0]);
        let mut small = [0u16; 2];
        utf16_into(&mut small, "\u{1F600}");
        assert_eq!(small, [0, 0]);
    }

    #[test]
    fn c8_cuts_on_a_char_boundary() {
        let mut b = [0 as c_char; 3];
        c8_into(&mut b, "a\u{b5}");
        assert_eq!(b, [b'a' as c_char, 0, 0]);
    }

    #[test]
    fn units_follow_group_paths() {
        let p = |g: &str| ParamDescriptor {
            group: g.to_owned(),
            ..ParamDescriptor::default()
        };
        let (units, map) = build_units(&[p(""), p("Osc/A"), p("Osc"), p("Osc/A")]);
        assert_eq!(units.len(), 3);
        assert_eq!(map, vec![0, 2, 1, 2]);
        assert_eq!(units[2].parent, 1);
    }
}
