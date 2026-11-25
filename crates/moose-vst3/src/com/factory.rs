//! `IPluginFactory` / `IPluginFactory2` / `IPluginFactory3`.

use std::ffi::c_void;
use std::sync::{Mutex, PoisonError};

use vst3::Steinberg::PClassInfo_::ClassCardinality_::kManyInstances;
use vst3::Steinberg::PFactoryInfo_::FactoryFlags_::kUnicode;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComRef, ComWrapper};

use super::component::{Component, Host};
use super::{Registration, c8_into, registration, utf16_into};

/// The SDK the bindings were generated from.
const SDK_VERSION: &str = "VST 3.8.0";
/// Steinberg VST3 3.7.5+ category string (`kPluginCompatibilityClass`).
const COMPATIBILITY_CATEGORY: &str = "Plugin Compatibility Class";

/// Non-empty compatibility JSON declared by the plugin, if any.
fn compatibility_json(reg: &'static Registration) -> Option<&'static [u8]> {
    reg.desc
        .compatibility_json
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::as_bytes)
}

/// Deterministic, collision-free sibling CID for the compatibility class.
pub(super) fn compatibility_cid(component_cid: [u8; 16]) -> [u8; 16] {
    let mut cid = component_cid;
    cid[0] ^= 0x5A;
    cid[15] = cid[15].wrapping_add(1);
    cid
}

struct Compatibility {
    json: &'static [u8],
}

impl Class for Compatibility {
    type Interfaces = (IPluginCompatibility,);
}

impl IPluginCompatibilityTrait for Compatibility {
    unsafe fn getCompatibilityJSON(&self, stream: *mut IBStream) -> tresult {
        // SAFETY: The host passes `stream` as null or a live `IBStream` for this call; `from_raw`
        // maps null to `None`.
        let Some(stream) = (unsafe { ComRef::from_raw(stream) }) else {
            return kInvalidArgument;
        };
        let mut written = 0usize;
        while written < self.json.len() {
            let chunk = (self.json.len() - written).min(i32::MAX as usize);
            let mut wrote: int32 = 0;
            // SAFETY: `stream` is live for this call. The source pointer covers `chunk` readable
            // bytes of `self.json` (the `*mut` cast only matches the C signature; `write` reads),
            // and `wrote` is a writable local.
            let rc = unsafe {
                stream.write(
                    self.json[written..].as_ptr().cast_mut().cast(),
                    chunk as int32,
                    &raw mut wrote,
                )
            };
            if rc != kResultOk || wrote <= 0 {
                return kResultFalse;
            }
            written += wrote as usize;
        }
        kResultOk
    }
}

pub(super) struct Factory {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    run_loop: Mutex<Option<ComPtr<Linux::IRunLoop>>>,
    #[cfg(target_os = "linux")]
    context: Mutex<Option<ComPtr<FUnknown>>>,
    #[cfg(test)]
    reg_override: Option<&'static Registration>,
}

impl Factory {
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn reg(&self) -> Option<&'static Registration> {
        #[cfg(test)]
        if let Some(reg) = self.reg_override {
            return Some(reg);
        }
        registration()
    }
}

impl Class for Factory {
    type Interfaces = (IPluginFactory3,);
}

pub(super) fn new_factory() -> *mut c_void {
    ComWrapper::new(Factory {
        run_loop: Mutex::new(None),
        #[cfg(target_os = "linux")]
        context: Mutex::new(None),
        #[cfg(test)]
        reg_override: None,
    })
    .to_com_ptr::<IPluginFactory3>()
    .map_or(std::ptr::null_mut(), |p| p.into_raw().cast())
}

#[cfg(test)]
pub(super) fn new_test_factory(reg: &'static Registration) -> ComWrapper<Factory> {
    ComWrapper::new(Factory {
        run_loop: Mutex::new(None),
        #[cfg(target_os = "linux")]
        context: Mutex::new(None),
        reg_override: Some(reg),
    })
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        // SAFETY: The host passes `info` as null or a `PFactoryInfo` slot for this call; `as_mut`
        // maps null to `None`.
        let (Some(info), Some(reg)) = (unsafe { info.as_mut() }, self.reg()) else {
            return kInvalidArgument;
        };
        // SAFETY: `PFactoryInfo` is plain integers and char arrays, so all-zero is a valid value;
        // the assignment drops nothing.
        *info = unsafe { std::mem::zeroed() };
        c8_into(&mut info.vendor, &reg.desc.vendor);
        c8_into(&mut info.url, &reg.desc.url);
        c8_into(&mut info.email, &reg.desc.email);
        info.flags = kUnicode as i32;
        kResultOk
    }

    unsafe fn countClasses(&self) -> int32 {
        match self.reg() {
            None => 0,
            Some(reg) => 1 + i32::from(compatibility_json(reg).is_some()),
        }
    }

    unsafe fn getClassInfo(&self, index: int32, info: *mut PClassInfo) -> tresult {
        // SAFETY: The host passes `info` as null or a `PClassInfo` slot for this call; `as_mut`
        // maps null to `None`.
        let (Some(info), Some(reg)) = (unsafe { info.as_mut() }, self.reg()) else {
            return kInvalidArgument;
        };
        let has_compat = compatibility_json(reg).is_some();
        if index != 0 && !(index == 1 && has_compat) {
            return kInvalidArgument;
        }
        // SAFETY: `PClassInfo` is plain integers and char arrays, so all-zero is a valid value.
        *info = unsafe { std::mem::zeroed() };
        info.cardinality = kManyInstances as i32;
        if index == 0 {
            info.cid = reg.desc.cid.map(|b| b as i8);
            c8_into(&mut info.category, &reg.desc.category);
        } else {
            info.cid = compatibility_cid(reg.desc.cid).map(|b| b as i8);
            c8_into(&mut info.category, COMPATIBILITY_CATEGORY);
        }
        c8_into(&mut info.name, &reg.desc.name);
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: FIDString,
        iid: FIDString,
        obj: *mut *mut c_void,
    ) -> tresult {
        let Some(reg) = self.reg() else {
            return kInvalidArgument;
        };
        if cid.is_null() || iid.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: `obj` is non-null (checked above) and is the caller's out-slot.
        unsafe { *obj = std::ptr::null_mut() };
        // SAFETY: `cid` is non-null, and a VST3 `FIDString` class id is a 16-byte TUID.
        let cid = unsafe { std::slice::from_raw_parts(cid.cast::<u8>(), 16) };
        // SAFETY: `iid` is non-null and points at a 16-byte TUID; `[u8; 16]` has alignment 1.
        let iid = unsafe { &*iid.cast::<[u8; 16]>() };
        if let Some(json) = compatibility_json(reg)
            && cid == compatibility_cid(reg.desc.cid)
        {
            let compat = ComWrapper::new(Compatibility { json });
            let Some(unknown) = compat.to_com_ptr::<FUnknown>() else {
                return kNoInterface;
            };
            // SAFETY: `unknown` is a live `ComPtr` we own, and `iid` is a valid 16-byte id. On
            // success the returned pointer carries its own reference.
            return match unsafe {
                <FUnknown as vst3::com_scrape_types::Unknown>::query_interface(
                    unknown.as_ptr(),
                    iid,
                )
            } {
                Some(p) => {
                    // SAFETY: `obj` is the non-null out-slot; ownership of the reference
                    // `query_interface` added passes to the caller.
                    unsafe { *obj = p };
                    kResultOk
                }
                None => kNoInterface,
            };
        }
        if cid != reg.desc.cid {
            return kResultFalse;
        }
        let run_loop = self
            .run_loop
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(host) = Host::new(reg, run_loop) else {
            return kResultFalse;
        };
        #[cfg(target_os = "linux")]
        if let Some(context) = &*self.context.lock().unwrap_or_else(PoisonError::into_inner) {
            // SAFETY: `context` is a `ComPtr` retained by the factory and held
            // under the lock for this call.
            unsafe { host.set_host_context(context.as_ptr()) };
        }
        let component = ComWrapper::new(Component { host });
        let Some(unknown) = component.to_com_ptr::<FUnknown>() else {
            return kNoInterface;
        };
        // SAFETY: `unknown` is a live `ComPtr` we own, and `iid` is a valid 16-byte id. On success
        // the returned pointer carries its own reference.
        match unsafe {
            <FUnknown as vst3::com_scrape_types::Unknown>::query_interface(unknown.as_ptr(), iid)
        } {
            Some(p) => {
                // SAFETY: `obj` is the non-null out-slot; ownership of the reference
                // `query_interface` added passes to the caller.
                unsafe { *obj = p };
                kResultOk
            }
            None => kNoInterface,
        }
    }
}

impl IPluginFactory2Trait for Factory {
    unsafe fn getClassInfo2(&self, index: int32, info: *mut PClassInfo2) -> tresult {
        // SAFETY: The host passes `info` as null or a `PClassInfo2` slot for this call; `as_mut`
        // maps null to `None`.
        let (Some(info), Some(reg)) = (unsafe { info.as_mut() }, self.reg()) else {
            return kInvalidArgument;
        };
        let has_compat = compatibility_json(reg).is_some();
        if index != 0 && !(index == 1 && has_compat) {
            return kInvalidArgument;
        }
        let d = &reg.desc;
        // SAFETY: `PClassInfo2` is plain integers and char arrays, so all-zero is a valid value.
        *info = unsafe { std::mem::zeroed() };
        info.cardinality = kManyInstances as i32;
        if index == 0 {
            info.cid = d.cid.map(|b| b as i8);
            c8_into(&mut info.category, &d.category);
            c8_into(&mut info.subCategories, &d.subcategories);
        } else {
            info.cid = compatibility_cid(d.cid).map(|b| b as i8);
            c8_into(&mut info.category, COMPATIBILITY_CATEGORY);
        }
        c8_into(&mut info.name, &d.name);
        info.classFlags = 0;
        c8_into(&mut info.vendor, &d.vendor);
        c8_into(&mut info.version, &d.version);
        c8_into(&mut info.sdkVersion, SDK_VERSION);
        kResultOk
    }
}

impl IPluginFactory3Trait for Factory {
    unsafe fn getClassInfoUnicode(&self, index: int32, info: *mut PClassInfoW) -> tresult {
        // SAFETY: The host passes `info` as null or a `PClassInfoW` slot for this call; `as_mut`
        // maps null to `None`.
        let (Some(info), Some(reg)) = (unsafe { info.as_mut() }, self.reg()) else {
            return kInvalidArgument;
        };
        let has_compat = compatibility_json(reg).is_some();
        if index != 0 && !(index == 1 && has_compat) {
            return kInvalidArgument;
        }
        let d = &reg.desc;
        // SAFETY: `PClassInfoW` is plain integers and char arrays, so all-zero is a valid value.
        *info = unsafe { std::mem::zeroed() };
        info.cardinality = kManyInstances as i32;
        if index == 0 {
            info.cid = d.cid.map(|b| b as i8);
            c8_into(&mut info.category, &d.category);
            c8_into(&mut info.subCategories, &d.subcategories);
        } else {
            info.cid = compatibility_cid(d.cid).map(|b| b as i8);
            c8_into(&mut info.category, COMPATIBILITY_CATEGORY);
        }
        utf16_into(&mut info.name, &d.name);
        info.classFlags = 0;
        utf16_into(&mut info.vendor, &d.vendor);
        utf16_into(&mut info.version, &d.version);
        utf16_into(&mut info.sdkVersion, SDK_VERSION);
        kResultOk
    }

    unsafe fn setHostContext(&self, #[allow(unused_variables)] context: *mut FUnknown) -> tresult {
        #[cfg(target_os = "linux")]
        {
            *self.run_loop.lock().unwrap_or_else(PoisonError::into_inner) =
                // SAFETY: `setHostContext`'s `context` is null or a host
                // object live for this call; `run_loop_of` keeps its own ref.
                unsafe { super::wake::linux::run_loop_of(context) };
            *self.context.lock().unwrap_or_else(PoisonError::into_inner) =
                // SAFETY: Same live-or-null `context`; `retain` takes the factory's own reference.
                unsafe { super::retain(context) };
        }
        kResultOk
    }
}
