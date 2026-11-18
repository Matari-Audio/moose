//! `IPluginFactory` / `IPluginFactory2` / `IPluginFactory3`.

use std::ffi::c_void;
use std::sync::{Mutex, PoisonError};

use vst3::Steinberg::PClassInfo_::ClassCardinality_::kManyInstances;
use vst3::Steinberg::PFactoryInfo_::FactoryFlags_::kUnicode;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper};

use super::component::{Component, Host};
use super::{c8_into, cstr, registration, utf16_into};

/// The SDK the bindings were generated from.
const SDK_VERSION: &str = "VST 3.8.0";

pub(super) struct Factory {
    /// Linux: the host run loop from `setHostContext`, handed to new
    /// instances for their persist wake.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    run_loop: Mutex<Option<ComPtr<Linux::IRunLoop>>>,
}

impl Class for Factory {
    type Interfaces = (IPluginFactory3,);
}

pub(super) fn new_factory() -> *mut c_void {
    ComWrapper::new(Factory {
        run_loop: Mutex::new(None),
    })
    .to_com_ptr::<IPluginFactory3>()
    .map_or(std::ptr::null_mut(), |p| p.into_raw().cast())
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        let (Some(info), Some(reg)) = (unsafe { info.as_mut() }, registration()) else {
            return kInvalidArgument;
        };
        *info = unsafe { std::mem::zeroed() };
        c8_into(&mut info.vendor, unsafe { cstr(reg.desc.vendor) });
        c8_into(&mut info.url, unsafe { cstr(reg.desc.url) });
        c8_into(&mut info.email, unsafe { cstr(reg.desc.email) });
        info.flags = kUnicode as i32;
        kResultOk
    }

    unsafe fn countClasses(&self) -> int32 {
        i32::from(registration().is_some())
    }

    unsafe fn getClassInfo(&self, index: int32, info: *mut PClassInfo) -> tresult {
        let (Some(info), Some(reg)) = (unsafe { info.as_mut() }, registration()) else {
            return kInvalidArgument;
        };
        if index != 0 {
            return kInvalidArgument;
        }
        *info = unsafe { std::mem::zeroed() };
        info.cid = reg.desc.cid.map(|b| b as i8);
        info.cardinality = kManyInstances as i32;
        c8_into(&mut info.category, unsafe { cstr(reg.desc.category) });
        c8_into(&mut info.name, unsafe { cstr(reg.desc.name) });
        kResultOk
    }

    unsafe fn createInstance(&self, cid: FIDString, iid: FIDString, obj: *mut *mut c_void) -> tresult {
        let Some(reg) = registration() else {
            return kInvalidArgument;
        };
        if cid.is_null() || iid.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        unsafe { *obj = std::ptr::null_mut() };
        let cid = unsafe { std::slice::from_raw_parts(cid.cast::<u8>(), 16) };
        if cid != reg.desc.cid {
            return kResultFalse;
        }
        let run_loop = self.run_loop.lock().unwrap_or_else(PoisonError::into_inner).clone();
        // A null instance means the plugin's create() panicked: fail
        // instantiation instead of handing the host an inert plugin.
        let Some(host) = Host::new(reg, run_loop) else {
            return kResultFalse;
        };
        let component = ComWrapper::new(Component { host });
        let Some(unknown) = component.to_com_ptr::<FUnknown>() else {
            return kNoInterface;
        };
        let iid = unsafe { &*iid.cast::<[u8; 16]>() };
        match unsafe { <FUnknown as vst3::com_scrape_types::Unknown>::query_interface(unknown.as_ptr(), iid) } {
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
        let (Some(info), Some(reg)) = (unsafe { info.as_mut() }, registration()) else {
            return kInvalidArgument;
        };
        if index != 0 {
            return kInvalidArgument;
        }
        let d = reg.desc;
        *info = unsafe { std::mem::zeroed() };
        info.cid = d.cid.map(|b| b as i8);
        info.cardinality = kManyInstances as i32;
        c8_into(&mut info.category, unsafe { cstr(d.category) });
        c8_into(&mut info.name, unsafe { cstr(d.name) });
        // Single component: processor and controller in one object.
        info.classFlags = 0;
        c8_into(&mut info.subCategories, unsafe { cstr(d.subcategories) });
        c8_into(&mut info.vendor, unsafe { cstr(d.vendor) });
        c8_into(&mut info.version, unsafe { cstr(d.version) });
        c8_into(&mut info.sdkVersion, SDK_VERSION);
        kResultOk
    }
}

impl IPluginFactory3Trait for Factory {
    unsafe fn getClassInfoUnicode(&self, index: int32, info: *mut PClassInfoW) -> tresult {
        let (Some(info), Some(reg)) = (unsafe { info.as_mut() }, registration()) else {
            return kInvalidArgument;
        };
        if index != 0 {
            return kInvalidArgument;
        }
        let d = reg.desc;
        *info = unsafe { std::mem::zeroed() };
        info.cid = d.cid.map(|b| b as i8);
        info.cardinality = kManyInstances as i32;
        c8_into(&mut info.category, unsafe { cstr(d.category) });
        utf16_into(&mut info.name, unsafe { cstr(d.name) });
        info.classFlags = 0;
        c8_into(&mut info.subCategories, unsafe { cstr(d.subcategories) });
        utf16_into(&mut info.vendor, unsafe { cstr(d.vendor) });
        utf16_into(&mut info.version, unsafe { cstr(d.version) });
        utf16_into(&mut info.sdkVersion, SDK_VERSION);
        kResultOk
    }

    unsafe fn setHostContext(&self, #[allow(unused_variables)] context: *mut FUnknown) -> tresult {
        #[cfg(target_os = "linux")]
        {
            *self.run_loop.lock().unwrap_or_else(PoisonError::into_inner) = super::wake::linux::run_loop_of(context);
        }
        kResultOk
    }
}
