//! Platform window bridging for baseview / wgpu.
//!
//! `create_wgpu_surface` consumes a baseview window context; baseview
//! and wgpu both speak raw-window-handle 0.6. Lives in `moose-gpu`
//! so the wgpu pipeline crate is self-contained; the per-OS
//! HWND/NSView lookups + DPI queries live next door in
//! `moose_gui::platform`.

#[cfg(target_os = "windows")]
pub(crate) fn current_module_hinstance() -> Option<std::num::NonZeroIsize> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    // SAFETY: `GetModuleHandleW(NULL)` returns the running EXE's HMODULE
    // without taking a reference.
    let hmodule = unsafe { GetModuleHandleW(std::ptr::null()) };
    std::num::NonZeroIsize::new(hmodule as isize)
}

/// wgpu backends for an editor surface embedded in a host-owned
/// child window. Mirror of `moose_gui::platform::editor_wgpu_backends`
/// (moose-gpu can't depend on moose-gui without a dep cycle). Windows
/// is DX12 (the only backend feature compiled in on Windows), macOS
/// is Metal, Linux keeps `PRIMARY`. Keep the two copies in sync.
#[must_use]
pub fn editor_wgpu_backends() -> wgpu::Backends {
    #[cfg(target_os = "windows")]
    {
        wgpu::Backends::DX12
    }
    #[cfg(target_os = "macos")]
    {
        wgpu::Backends::METAL
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        wgpu::Backends::PRIMARY
    }
}

/// `wgpu::InstanceDescriptor` for editor surfaces, pinning the DX12
/// shader compiler to **FXC**. Mirror of
/// `moose_gui::platform::editor_instance_descriptor` - see it for why
/// the wgpu-29 DX12 default (dynamic DXC) breaks inside Pro Tools.
/// Keep the two copies in sync.
#[must_use]
pub fn editor_instance_descriptor() -> wgpu::InstanceDescriptor {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = editor_wgpu_backends();
    desc.backend_options.dx12.shader_compiler = wgpu::Dx12Compiler::Fxc;
    desc
}

/// Create a wgpu surface for a baseview window (both speak
/// raw-window-handle 0.6).
///
/// # Safety
/// The window must outlive the returned surface.
#[cfg(not(target_os = "ios"))]
#[must_use]
pub unsafe fn create_wgpu_surface(
    instance: &wgpu::Instance,
    window: &(impl raw_window_handle::HasWindowHandle + raw_window_handle::HasDisplayHandle),
) -> Option<wgpu::Surface<'static>> {
    let target = wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: Some(window.display_handle().ok()?.as_raw()),
        raw_window_handle: window.window_handle().ok()?.as_raw(),
    };
    unsafe { instance.create_surface_unsafe(target) }.ok()
}
