//! VST3 COM objects on the official SDK bindings (`vst3` crate, generated
//! from the MIT `pluginterfaces` headers).
//!
//! The objects here own everything host-facing: interface layout, IIDs,
//! result codes, threading of host calls. Plugin behaviour stays in the
//! callback table that `lib.rs` registers (`ffi::Vst3Callbacks`).
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

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use vst3::Steinberg::{IBStream, IBStreamTrait, kResultFalse, kResultOk};
use vst3::{ComPtr, ComRef};

use crate::ffi::{Vst3Callbacks, Vst3ParamDescriptor, Vst3PluginDescriptor};

pub(crate) use component::Host;

/// Everything `register` hands over, plus the unit tree derived from it.
pub(crate) struct Registration {
    pub desc: &'static Vst3PluginDescriptor,
    pub cb: &'static Vst3Callbacks,
    pub params: &'static [Vst3ParamDescriptor],
    pub units: Vec<Unit>,
    /// `IUnitInfo` unit id per entry of `params`.
    pub param_unit: Vec<i32>,
}

pub(crate) struct Unit {
    pub id: i32,
    pub parent: i32,
    pub name: String,
}

// SAFETY: every pointer inside is to leaked, immutable registration data.
unsafe impl Send for Registration {}
unsafe impl Sync for Registration {}

static REGISTRATION: OnceLock<Registration> = OnceLock::new();

pub(crate) fn registration() -> Option<&'static Registration> {
    REGISTRATION.get()
}

pub(crate) fn register(
    desc: &'static Vst3PluginDescriptor,
    cb: &'static Vst3Callbacks,
    params: &'static [Vst3ParamDescriptor],
) {
    let (units, param_unit) = build_units(params);
    let _ = REGISTRATION.set(Registration {
        desc,
        cb,
        params,
        units,
        param_unit,
    });
}

/// One unit per distinct `/`-separated group path, root unit 0.
fn build_units(params: &[Vst3ParamDescriptor]) -> (Vec<Unit>, Vec<i32>) {
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
            for name in unsafe { cstr(p.group) }
                .split('/')
                .filter(|s| !s.is_empty())
            {
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

/// `&str` from a registration C string; empty for null or invalid UTF-8.
pub(crate) unsafe fn cstr<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        ""
    } else {
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
    while len < max && unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

// ---------------------------------------------------------------------------
// Host state streams
// ---------------------------------------------------------------------------

/// One bounded read from a host `IBStream` straight into the Rust parser.
/// Returns the byte count, or -1 on an error or an impossible count.
pub(crate) unsafe extern "C" fn read_state_chunk(
    stream: *mut c_void,
    dst: *mut u8,
    requested: u32,
) -> i64 {
    let Some(stream) = (unsafe { ComRef::from_raw(stream.cast::<IBStream>()) }) else {
        return -1;
    };
    if dst.is_null() || requested == 0 || requested > 8192 {
        return -1;
    }
    let mut count = 0;
    let result = unsafe { stream.read(dst.cast(), requested as i32, &raw mut count) };
    if count < 0 || count as u32 > requested || (result != kResultOk && result != kResultFalse) {
        return -1;
    }
    i64::from(count)
}

/// One bounded write to a host `IBStream`. Returns the byte count, or -1.
pub(crate) unsafe extern "C" fn write_state_chunk(
    stream: *mut c_void,
    src: *const u8,
    requested: u32,
) -> i64 {
    let Some(stream) = (unsafe { ComRef::from_raw(stream.cast::<IBStream>()) }) else {
        return -1;
    };
    if src.is_null() || requested == 0 || requested > 8192 {
        return -1;
    }
    let mut count = 0;
    let result = unsafe { stream.write(src.cast_mut().cast(), requested as i32, &raw mut count) };
    if count <= 0 || count as u32 > requested || result != kResultOk {
        return -1;
    }
    i64::from(count)
}

/// Write a whole blob, looping over partial writes. 1 when every byte
/// landed, 0 on an empty or oversized blob, no progress, a count past the
/// request, or an error before the end.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) unsafe extern "C" fn write_state_stream(
    stream: *mut c_void,
    data: *const u8,
    len: u32,
) -> i32 {
    let Some(stream) = (unsafe { ComRef::from_raw(stream.cast::<IBStream>()) }) else {
        return 0;
    };
    if data.is_null() || len == 0 || len > i32::MAX as u32 {
        return 0;
    }
    let mut offset = 0u32;
    while offset < len {
        let request = (len - offset).min(65536) as i32;
        let mut written = 0;
        let r = unsafe {
            stream.write(
                data.add(offset as usize).cast_mut().cast(),
                request,
                &raw mut written,
            )
        };
        if written <= 0 || written > request {
            return 0;
        }
        offset += written as u32;
        if r != kResultOk && offset < len {
            return 0;
        }
    }
    1
}

// ---------------------------------------------------------------------------
// Instance registry: Rust ctx -> live host object
// ---------------------------------------------------------------------------

/// Editor calls arrive with the Rust ctx, possibly on a worker thread and
/// possibly after the host released the component. A `Weak` per ctx lets
/// such a call pin the instance only while it is alive.
static INSTANCES: Mutex<Vec<(usize, Weak<Host>)>> = Mutex::new(Vec::new());

fn instances() -> std::sync::MutexGuard<'static, Vec<(usize, Weak<Host>)>> {
    INSTANCES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn register_instance(host: &Arc<Host>) {
    instances().push((host.ctx as usize, Arc::downgrade(host)));
}

pub(crate) fn unregister_instance(ctx: *mut c_void) {
    instances().retain(|(key, _)| *key != ctx as usize);
}

/// The live instance for `ctx`, held for the duration of the call.
pub(crate) fn pin(ctx: *mut c_void) -> Option<Arc<Host>> {
    instances()
        .iter()
        .find(|(key, _)| *key == ctx as usize)
        .and_then(|(_, weak)| weak.upgrade())
}

#[cfg(target_os = "windows")]
pub(crate) fn find_instance(pred: impl Fn(&Host) -> bool) -> Option<Arc<Host>> {
    let snapshot: Vec<_> = instances().iter().map(|(_, weak)| weak.clone()).collect();
    snapshot.iter().filter_map(Weak::upgrade).find(|h| pred(h))
}

// ---------------------------------------------------------------------------
// Calls from lib.rs
// ---------------------------------------------------------------------------

pub(crate) fn begin_edit(ctx: *mut c_void, id: u32) {
    if let Some(host) = pin(ctx) {
        host.submit(component::EditOp::Begin(id));
    }
}

pub(crate) fn perform_edit(ctx: *mut c_void, id: u32, normalized: f64) {
    if let Some(host) = pin(ctx) {
        host.submit(component::EditOp::Perform(id, normalized));
    }
}

pub(crate) fn end_edit(ctx: *mut c_void, id: u32) {
    if let Some(host) = pin(ctx) {
        host.submit(component::EditOp::End(id));
    }
}

pub(crate) fn request_resize(ctx: *mut c_void, w: u32, h: u32) -> i32 {
    pin(ctx).map_or(0, |host| host.submit(component::EditOp::Resize(w, h)))
}

pub(crate) fn mark_dirty(ctx: *mut c_void) {
    if let Some(host) = pin(ctx) {
        host.submit(component::EditOp::Dirty);
    }
}

pub(crate) fn group_edit(ctx: *mut c_void, edge: moose_core::editor::EditGroup) {
    if let Some(host) = pin(ctx) {
        host.submit(component::EditOp::Group(edge));
    }
}

pub(crate) fn mark_restart(ctx: *mut c_void, flags: i32) {
    component::mark_restart(ctx, flags);
}

/// `ComPtr` from an incoming interface pointer, adding a reference.
pub(crate) unsafe fn retain<I: vst3::Interface>(p: *mut I) -> Option<ComPtr<I>> {
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
        let p = |g: &'static CStr| Vst3ParamDescriptor {
            id: 0,
            name: c"".as_ptr(),
            short_name: c"".as_ptr(),
            units: c"".as_ptr(),
            min: 0.0,
            max: 1.0,
            default_normalized: 0.0,
            step_count: 0,
            flags: 0,
            group: g.as_ptr(),
        };
        let (units, map) = build_units(&[p(c""), p(c"Osc/A"), p(c"Osc"), p(c"Osc/A")]);
        assert_eq!(units.len(), 3);
        assert_eq!(map, vec![0, 2, 1, 2]);
        assert_eq!(units[2].parent, 1);
    }
}
