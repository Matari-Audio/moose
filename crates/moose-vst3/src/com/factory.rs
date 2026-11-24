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
        let Some(stream) = (unsafe { ComRef::from_raw(stream) }) else {
            return kInvalidArgument;
        };
        let mut written = 0usize;
        while written < self.json.len() {
            let chunk = (self.json.len() - written).min(i32::MAX as usize);
            let mut wrote: int32 = 0;
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
        let (Some(info), Some(reg)) = (unsafe { info.as_mut() }, self.reg()) else {
            return kInvalidArgument;
        };
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
        let (Some(info), Some(reg)) = (unsafe { info.as_mut() }, self.reg()) else {
            return kInvalidArgument;
        };
        let has_compat = compatibility_json(reg).is_some();
        if index != 0 && !(index == 1 && has_compat) {
            return kInvalidArgument;
        }
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
        unsafe { *obj = std::ptr::null_mut() };
        let cid = unsafe { std::slice::from_raw_parts(cid.cast::<u8>(), 16) };
        let iid = unsafe { &*iid.cast::<[u8; 16]>() };
        if let Some(json) = compatibility_json(reg)
            && cid == compatibility_cid(reg.desc.cid)
        {
            let compat = ComWrapper::new(Compatibility { json });
            let Some(unknown) = compat.to_com_ptr::<FUnknown>() else {
                return kNoInterface;
            };
            return match unsafe {
                <FUnknown as vst3::com_scrape_types::Unknown>::query_interface(
                    unknown.as_ptr(),
                    iid,
                )
            } {
                Some(p) => {
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
            host.set_host_context(context.as_ptr());
        }
        let component = ComWrapper::new(Component { host });
        let Some(unknown) = component.to_com_ptr::<FUnknown>() else {
            return kNoInterface;
        };
        match unsafe {
            <FUnknown as vst3::com_scrape_types::Unknown>::query_interface(unknown.as_ptr(), iid)
        } {
            Some(p) => {
                unsafe { *obj = p };
                kResultOk
            }
            None => kNoInterface,
        }
    }
}

impl IPluginFactory2Trait for Factory {
    unsafe fn getClassInfo2(&self, index: int32, info: *mut PClassInfo2) -> tresult {
        let (Some(info), Some(reg)) = (unsafe { info.as_mut() }, self.reg()) else {
            return kInvalidArgument;
        };
        let has_compat = compatibility_json(reg).is_some();
        if index != 0 && !(index == 1 && has_compat) {
            return kInvalidArgument;
        }
        let d = &reg.desc;
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
        let (Some(info), Some(reg)) = (unsafe { info.as_mut() }, self.reg()) else {
            return kInvalidArgument;
        };
        let has_compat = compatibility_json(reg).is_some();
        if index != 0 && !(index == 1 && has_compat) {
            return kInvalidArgument;
        }
        let d = &reg.desc;
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
                super::wake::linux::run_loop_of(context);
            *self.context.lock().unwrap_or_else(PoisonError::into_inner) =
                unsafe { super::retain(context) };
        }
        kResultOk
    }
}
