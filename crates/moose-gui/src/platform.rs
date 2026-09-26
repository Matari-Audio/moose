//! Platform window bridging for baseview.
//!
//! Bridges moose's `RawWindowHandle` to raw-window-handle 0.6 (what
//! baseview takes), and provides the scale policy, scale factor querying
//! and wgpu surface creation.

use moose_core::editor::RawWindowHandle;
use raw_window_handle as rwh;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Newtype bridging moose's `RawWindowHandle` to a raw-window-handle 0.6
/// window handle (what baseview takes as a parent).
pub struct ParentWindow(pub RawWindowHandle);

impl rwh::HasWindowHandle for ParentWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let null = || rwh::HandleError::Unavailable;
        let raw = match self.0 {
            RawWindowHandle::AppKit(ptr) => rwh::RawWindowHandle::AppKit(
                rwh::AppKitWindowHandle::new(std::ptr::NonNull::new(ptr).ok_or_else(null)?),
            ),
            RawWindowHandle::UiKit(ptr) => rwh::RawWindowHandle::UiKit(
                rwh::UiKitWindowHandle::new(std::ptr::NonNull::new(ptr).ok_or_else(null)?),
            ),
            RawWindowHandle::Win32(ptr) => {
                rwh::RawWindowHandle::Win32(rwh::Win32WindowHandle::new(
                    std::num::NonZeroIsize::new(ptr as isize).ok_or_else(null)?,
                ))
            }
            RawWindowHandle::X11(id) => {
                let id = u32::try_from(id)
                    .ok()
                    .and_then(std::num::NonZeroU32::new)
                    .ok_or_else(null)?;
                rwh::RawWindowHandle::Xcb(rwh::XcbWindowHandle::new(id))
            }
        };
        // SAFETY: the host keeps the parent alive while the editor is open.
        Ok(unsafe { rwh::WindowHandle::borrow_raw(raw) })
    }
}

/// Query the backing scale factor from the parent `NSView`'s window.
#[cfg(target_os = "macos")]
#[must_use]
pub fn query_backing_scale(parent: &RawWindowHandle) -> f64 {
    use objc::{msg_send, sel, sel_impl};

    let ns_view_ptr = match parent {
        RawWindowHandle::AppKit(ptr) => *ptr,
        _ => return 1.0,
    };

    if ns_view_ptr.is_null() {
        return 1.0;
    }

    unsafe {
        let ns_view = ns_view_ptr.cast::<objc::runtime::Object>();
        let window: *mut objc::runtime::Object = msg_send![ns_view, window];
        let scale: f64 = if window.is_null() {
            let screen: *mut objc::runtime::Object = msg_send![objc::class!(NSScreen), mainScreen];
            if screen.is_null() {
                2.0
            } else {
                msg_send![screen, backingScaleFactor]
            }
        } else {
            msg_send![window, backingScaleFactor]
        };
        if scale < 1.0 { 1.0 } else { scale }
    }
}

#[cfg(target_os = "windows")]
#[must_use]
pub fn query_backing_scale(parent: &RawWindowHandle) -> f64 {
    let hwnd = match parent {
        RawWindowHandle::Win32(ptr) => *ptr,
        _ => return 1.0,
    };
    win32_dpi_scale(hwnd)
}

#[cfg(target_os = "linux")]
#[must_use]
pub fn query_backing_scale(_parent: &RawWindowHandle) -> f64 {
    main_screen_scale()
}

#[cfg(target_os = "ios")]
#[must_use]
pub fn query_backing_scale(parent: &RawWindowHandle) -> f64 {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let ui_view_ptr = match parent {
        RawWindowHandle::UiKit(ptr) => *ptr,
        _ => return 1.0,
    };
    if ui_view_ptr.is_null() {
        return main_screen_scale();
    }
    // SAFETY: UIView is a UIKit class; `contentScaleFactor` is a
    // public Objective-C property returning CGFloat (= f64 on
    // arm64). Called on the main thread per UIKit's threading
    // rule, which is also where AUv3 view controllers live.
    unsafe {
        let ui_view: *mut AnyObject = ui_view_ptr.cast();
        let scale: f64 = msg_send![ui_view, contentScaleFactor];
        if scale > 0.0 { scale } else { 1.0 }
    }
}

#[cfg(target_os = "ios")]
#[must_use]
pub fn main_screen_scale() -> f64 {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    // SAFETY: `+[UIScreen mainScreen]` is documented to return the
    // process's primary screen on the main thread.
    unsafe {
        let Some(cls) = AnyClass::get(c"UIScreen") else {
            return 1.0;
        };
        let screen: *mut AnyObject = msg_send![cls, mainScreen];
        if screen.is_null() {
            return 1.0;
        }
        let scale: f64 = msg_send![screen, scale];
        if scale > 0.0 { scale } else { 1.0 }
    }
}

/// Query the main screen's backing scale factor (no parent window needed).
#[cfg(target_os = "macos")]
#[must_use]
pub fn main_screen_scale() -> f64 {
    use objc::{msg_send, sel, sel_impl};
    unsafe {
        let screen: *mut objc::runtime::Object = msg_send![objc::class!(NSScreen), mainScreen];
        if screen.is_null() {
            1.0
        } else {
            let scale: f64 = msg_send![screen, backingScaleFactor];
            if scale < 1.0 { 1.0 } else { scale }
        }
    }
}

#[cfg(target_os = "windows")]
#[must_use]
pub fn main_screen_scale() -> f64 {
    win32_dpi_scale(std::ptr::null_mut())
}

// `reanchor_to_superview_top` and `reanchor_all_children_to_top`
// live in the `moose-gui-utils` crate so backends that don't pull
// `moose-gui` (vizia) can still get at them. Re-exported here for
// existing `moose_gui::platform::...` call sites.
pub use moose_gui_utils::{
    reanchor_all_children_to_top, reanchor_to_superview_top, should_skip_frame,
};

/// Shared, mutable editor scale factor.
///
/// Single source of truth for the live content-scale of an open plugin
/// window. Each GUI backend (egui / iced / slint) constructs one in
/// `Editor::open`, stores it on the editor for `set_scale_factor` to
/// write through, and hands a clone to its baseview `WindowHandler` so
/// the render thread can pick up changes between frames.
///
/// Two writers, one reader-per-frame:
/// - `Editor::set_scale_factor` (host → editor, e.g. CLAP `set_scale`,
///   VST3 Windows `IPlugViewContentScaleSupport`).
/// - `WindowEvent::Resized` (baseview → handler, fired when the OS
///   reports a new content scale, e.g. dragging the window across
///   monitors with different DPIs).
///
/// Most-recent-write wins. The handler tracks a `last_applied_scale`
/// alongside its `EditorScale` clone and, when it observes a divergence
/// at frame start, recomputes physical sizes and reconfigures its
/// surface / renderer.
/// Paces editor paints to the compositor's measured consumption rate.
///
/// A child-window swapchain acquire (`get_current_texture`) blocks
/// until the compositor frees a frame slot, and a host may compose an
/// embedded editor window far below the editor's tick rate (REAPER on
/// Windows composes FX child windows at ~13 Hz). Painting every tick
/// then parks the host's GUI thread in that wait - measured at 74 ms
/// of every 77 ms frame, which saturates the host's whole UI and reads
/// as the DAW hanging. Editors feed each paint's measured acquire wait
/// into [`Self::record_acquire`] and skip the tick while
/// [`Self::should_hold`] is true, converging on the rate the
/// compositor actually displays (which is all the user ever saw).
///
/// The pace is a decayed maximum: a successfully paced paint measures
/// a ~0 ms acquire, so pacing by the last wait alone oscillates
/// (paced, unpaced, paced, ...). Holding the estimate through the
/// zeros and shrinking it ~15% per paint lets it track a compositor
/// that genuinely speeds up within roughly a second.
#[derive(Debug, Default)]
pub struct PaintPacer {
    not_before: Option<std::time::Instant>,
    pace: std::time::Duration,
}

impl PaintPacer {
    /// Acquire waits below this are treated as "compositor keeps up" -
    /// no pacing.
    const MIN_PACE: std::time::Duration = std::time::Duration::from_millis(8);
    /// Ceiling on the hold so a pathological acquire (wgpu's internal
    /// timeout is ~1 s) can't park the editor for a full second.
    const MAX_PACE: std::time::Duration = std::time::Duration::from_millis(250);

    /// Whether this tick's paint should be skipped: the previous
    /// acquire showed the compositor hasn't caught up yet.
    #[must_use]
    pub fn should_hold(&self) -> bool {
        self.not_before
            .is_some_and(|t| std::time::Instant::now() < t)
    }

    /// Record how long a paint's swapchain acquire blocked, scheduling
    /// the earliest next paint accordingly.
    pub fn record_acquire(&mut self, wait: std::time::Duration) {
        self.pace = wait.max(self.pace.mul_f32(0.85));
        self.not_before = (self.pace >= Self::MIN_PACE)
            .then(|| std::time::Instant::now() + self.pace.min(Self::MAX_PACE));
    }
}

#[derive(Clone)]
pub struct EditorScale {
    inner: Arc<AtomicU64>,
    /// Set once the host announced a content scale that the platform
    /// honours (see [`host_scale_override`]). From then on the host owns
    /// the value and OS scale reports no longer overwrite it.
    host_set: Arc<AtomicBool>,
}

impl EditorScale {
    /// Construct with an initial scale. Non-finite or non-positive
    /// values clamp to 1.0 so callers never have to defend against
    /// `0.0 * size` collapsing the surface.
    #[must_use]
    pub fn new(initial: f64) -> Self {
        let v = if initial.is_finite() && initial > 0.0 {
            initial
        } else {
            1.0
        };
        Self {
            inner: Arc::new(AtomicU64::new(v.to_bits())),
            host_set: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Record a content scale reported by the host (CLAP `set_scale`,
    /// VST3 `setContentScaleFactor`, `cargo moose screenshot --scale`).
    /// Where the platform takes host scales the host then owns the value;
    /// on macOS (logical `AppKit` coordinates) it only seeds the value until
    /// the window reports its backing scale.
    pub fn set_from_host(&self, scale: f64) {
        if host_scale_override(Some(scale)).is_some() {
            self.host_set.store(true, Ordering::Relaxed);
        }
        self.set(scale);
    }

    /// Record a scale reported by the OS for the editor's window (initial
    /// window scale, DPI / monitor change). Ignored once the host owns the
    /// scale, so the host value is never overwritten by the window DPI.
    pub fn set_from_os(&self, scale: f64) {
        if !self.host_set.load(Ordering::Relaxed) {
            self.set(scale);
        }
    }

    /// The scale baseview should be pinned to: the host scale once the host
    /// announced one, else `None` (follow the OS).
    #[must_use]
    pub fn host_override(&self) -> Option<f64> {
        self.host_set.load(Ordering::Relaxed).then(|| self.get())
    }

    /// [`moose_core::editor::Editor::window_scale`] for an editor whose
    /// window is `open`: the current scale, except on macOS.
    #[must_use]
    pub fn window_scale(&self, open: bool) -> Option<f64> {
        (open && !cfg!(target_os = "macos")).then(|| self.get())
    }

    /// Read the current scale.
    #[must_use]
    pub fn get(&self) -> f64 {
        f64::from_bits(self.inner.load(Ordering::Relaxed))
    }

    /// Read the current scale, narrowed to `f32` for renderer / DSP
    /// use. Display scales never exceed 4.0 in practice, so the f64
    /// → f32 narrowing is invisible.
    #[allow(clippy::cast_possible_truncation)]
    #[must_use]
    pub fn get_f32(&self) -> f32 {
        self.get() as f32
    }

    /// Update the current scale. Non-finite or non-positive values are
    /// silently dropped - callers are forwarding numbers from hosts /
    /// `info.scale()` where a bad value is a host bug, not something
    /// to propagate into the surface config.
    pub fn set(&self, scale: f64) {
        if scale.is_finite() && scale > 0.0 {
            self.inner.store(scale.to_bits(), Ordering::Relaxed);
        } else {
            // Surface the upstream bug at least in debug builds so a
            // host that's emitting bad scales doesn't get silently
            // ignored. Production builds drop quietly to keep the
            // editor running.
            log::warn!(
                "EditorScale::set ignored a bad value ({scale}); \
                 expected finite, positive f64",
            );
        }
    }

    /// Pick up a host-driven scale change since the last frame.
    ///
    /// Reads the current scale (narrowed to `f32`) and compares it
    /// bit-identically against `last`. When the value moved, updates
    /// `last` and returns `Some(cur)`; otherwise returns `None`.
    ///
    /// Used by every editor backend's per-frame loop to gate surface /
    /// renderer reconfiguration on actual host scale events. Bit-equality
    /// is the correct semantics - the cell is written verbatim from
    /// host callbacks, never through accumulating arithmetic, so an
    /// epsilon-based check would either thrash on noise (there is
    /// none) or miss a legitimate `1.0 → 1.0001` host signal.
    #[allow(clippy::cast_possible_truncation, clippy::float_cmp)]
    pub fn take_change(&self, last: &mut f32) -> Option<f32> {
        let cur = self.get() as f32;
        if cur == *last {
            None
        } else {
            *last = cur;
            Some(cur)
        }
    }
}

/// Convert a logical extent (in points) to physical pixels.
///
/// Standardised rounding policy across every moose GUI backend:
/// round to nearest, then clamp the result to `1` so a degenerate
/// `0 × scale` doesn't collapse a wgpu surface (`width: 0` is a
/// validation error). The `logical.max(1)` guard handles the
/// converse - a zero-logical caller can't multiply through to `0`
/// before the round.
///
/// Canonical definition lives in `moose-gui-types` so `moose-gpu`'s
/// `WgpuBackend` can call it without a `moose-gui` dep (cycle); the
/// re-export below preserves the historical `moose_gui::to_physical_px`
/// path.
pub use moose_gui_types::to_physical_px;

/// Cached display scale factor on Linux, stored as f64 bits. Zero means unset.
///
/// Linux has no safe synchronous DPI query from plugin code - the authoritative
/// value is read by baseview internally (from `Xft.dpi` with a screen-geometry
/// fallback) and delivered via `WindowEvent::Resized::info.scale()` once the
/// window is live. We cache the first value an editor sees there so that later
/// pre-window `main_screen_scale()` calls (e.g. the next editor's `::new`)
/// return something useful instead of 1.0.
#[cfg(target_os = "linux")]
static LINUX_SCALE_BITS: AtomicU64 = AtomicU64::new(0);

/// Record the display scale factor observed from baseview on Linux. Editors
/// should call this from their `WindowEvent::Resized` handlers so subsequent
/// pre-window queries match what baseview is delivering. No-op on non-Linux.
pub fn note_linux_scale_factor(scale: f64) {
    #[cfg(target_os = "linux")]
    {
        if scale.is_finite() && scale > 0.0 {
            LINUX_SCALE_BITS.store(scale.to_bits(), Ordering::Relaxed);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = scale;
    }
}

/// The scale policy for an editor window: the host's content scale when it
/// sent one, otherwise the OS scale. Returns the value to pin baseview to
/// (`WindowSettings::with_scale_factor_override`), or `None` to follow the
/// OS scale.
///
/// - Windows / Linux: the host scale wins when given (and is valid). Without
///   one, baseview uses the window DPI (Windows) or `Xft.dpi` (X11). The two
///   are never multiplied.
/// - macOS: always `None`. `AppKit` coordinates are logical and the backing
///   scale comes from the window; CLAP `set_scale` is refused there.
#[must_use]
pub fn host_scale_override(host_scale: Option<f64>) -> Option<f64> {
    if cfg!(target_os = "macos") {
        return None;
    }
    host_scale.filter(|s| s.is_finite() && *s > 0.0)
}

#[cfg(target_os = "linux")]
pub fn main_screen_scale() -> f64 {
    // Priority: MOOSE_SCALE env var (dev/test override) → cached scale
    // observed from baseview → 1.0 fallback. No side-channel Xlib calls -
    // those crashed inside NVIDIA's Vulkan driver when invoked from the
    // render thread.
    if let Ok(s) = std::env::var("MOOSE_SCALE").or_else(|_| std::env::var("TRUCE_SCALE"))
        && let Ok(v) = s.parse::<f64>()
        && v.is_finite()
        && v > 0.0
    {
        return v;
    }
    let bits = LINUX_SCALE_BITS.load(Ordering::Relaxed);
    if bits == 0 {
        return 1.0;
    }
    let v = f64::from_bits(bits);
    if v.is_finite() && v > 0.0 { v } else { 1.0 }
}

/// Query the DPI scale factor on Windows.
/// If `hwnd` is non-null, queries per-window DPI; otherwise queries the system DPI.
#[cfg(target_os = "windows")]
fn win32_dpi_scale(hwnd: *mut std::ffi::c_void) -> f64 {
    use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
    // Default DPI is 96; scale = actual_dpi / 96.
    const DEFAULT_DPI: u32 = 96;

    // SAFETY: pure queries; a stale HWND makes GetDpiForWindow return 0.
    let dpi = match unsafe { GetDpiForWindow(hwnd) } {
        0 => unsafe { GetDpiForSystem() },
        d => d,
    };
    if dpi == 0 {
        1.0
    } else {
        f64::from(dpi) / f64::from(DEFAULT_DPI)
    }
}

/// Physical client-area size of a Win32 window, straight from
/// `GetClientRect`. The authoritative answer to "how many pixels does
/// the swapchain have to cover" - unlike `to_physical_px(logical,
/// scale)`, which is a prediction that can diverge from what the host
/// actually sized the child window to. `None` for non-Win32 handles,
/// a dead HWND, or an empty rect.
#[cfg(target_os = "windows")]
#[must_use]
pub fn win32_client_size(window: &impl rwh::HasWindowHandle) -> Option<(u32, u32)> {
    use windows_sys::Win32::{Foundation::RECT, UI::WindowsAndMessaging::GetClientRect};

    let rwh::RawWindowHandle::Win32(h) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: pure state query on a window handle baseview owns for
    // the editor's lifetime, called from the GUI thread that owns the
    // HWND.
    if unsafe { GetClientRect(h.hwnd.get() as _, &raw mut rect) } == 0 {
        return None;
    }
    // Client coordinates put left/top at 0; right/bottom are the size.
    let w = u32::try_from(rect.right).ok()?;
    let hgt = u32::try_from(rect.bottom).ok()?;
    if w == 0 || hgt == 0 {
        return None;
    }
    Some((w, hgt))
}

#[cfg(target_os = "windows")]
fn current_module_hinstance() -> Option<std::num::NonZeroIsize> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    // SAFETY: `GetModuleHandleW(NULL)` returns the running EXE's HMODULE
    // without taking a reference.
    let hmodule = unsafe { GetModuleHandleW(std::ptr::null()) };
    std::num::NonZeroIsize::new(hmodule as isize)
}

/// wgpu backends to use for an editor that presents into a
/// host-owned child window. macOS is Metal-only; Windows is DX12
/// (the only backend feature moose-gui/moose-gpu compile in on
/// Windows - see their `Cargo.toml`); Linux keeps `PRIMARY`.
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

/// `wgpu::InstanceDescriptor` for editor surfaces, with the DX12
/// shader compiler pinned to **FXC**.
///
/// wgpu 29 defaults DX12 to a dynamically-loaded **DXC**
/// (`dxcompiler.dll`). When a host process has already loaded its own
/// incompatible `dxcompiler.dll` - Pro Tools does - wgpu's
/// `DxcCreateInstance` returns `E_NOINTERFACE` and the *entire* DX12
/// backend fails to initialise, leaving the instance with zero
/// adapters: a blank editor (egui / built-in) or a panic on the
/// `.expect` (slint). FXC (`d3dcompiler_47.dll`, always present on
/// Windows, never conflicts) sidesteps it. wgpu 0.19 (iced)
/// defaulted to FXC, which is why iced was never affected.
#[must_use]
pub fn editor_instance_descriptor() -> wgpu::InstanceDescriptor {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = editor_wgpu_backends();
    desc.backend_options.dx12.shader_compiler = wgpu::Dx12Compiler::Fxc;
    desc
}

/// Create a wgpu surface for a baseview window.
///
/// baseview and wgpu both speak raw-window-handle 0.6, so this is a
/// straight hand-off, except on Windows, where the surface is created from
/// the HWND (see [`create_wgpu_surface_from_hwnd`]).
///
/// # Safety
/// The window must outlive the returned surface.
#[cfg(not(target_os = "ios"))]
#[must_use]
pub unsafe fn create_wgpu_surface(
    instance: &wgpu::Instance,
    window: &(impl rwh::HasWindowHandle + rwh::HasDisplayHandle),
) -> Option<wgpu::Surface<'static>> {
    let raw_window_handle = window.window_handle().ok()?.as_raw();
    #[cfg(target_os = "windows")]
    {
        let rwh::RawWindowHandle::Win32(handle) = raw_window_handle else {
            return None;
        };
        unsafe { create_wgpu_surface_from_hwnd(instance, handle.hwnd.get()) }
    }
    #[cfg(not(target_os = "windows"))]
    unsafe {
        let surface_target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(window.display_handle().ok()?.as_raw()),
            raw_window_handle,
        };
        instance.create_surface_unsafe(surface_target).ok()
    }
}

/// Windows-only variant of [`create_wgpu_surface`] that takes the raw
/// HWND value instead of a window reference. An `isize` is `Send`,
/// so callers can create the surface on a worker thread and keep the
/// host's GUI thread free while the graphics driver initializes (a
/// wedged driver can block device/surface creation indefinitely).
///
/// # Safety
/// `hwnd` must be a live window handle that outlives the returned
/// surface. If the window is destroyed first, swapchain creation fails
/// with a driver error (DXGI validates the HWND) rather than UB.
#[cfg(target_os = "windows")]
#[must_use]
pub unsafe fn create_wgpu_surface_from_hwnd(
    instance: &wgpu::Instance,
    hwnd: isize,
) -> Option<wgpu::Surface<'static>> {
    let mut win32 = wgpu::rwh::Win32WindowHandle::new(std::num::NonZeroIsize::new(hwnd)?);
    // wgpu's Vulkan backend requires `hinstance` to be set
    // (`vkCreateWin32SurfaceKHR` rejects a null HINSTANCE).
    win32.hinstance = current_module_hinstance();
    let surface_target = wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: Some(wgpu::rwh::RawDisplayHandle::Windows(
            wgpu::rwh::WindowsDisplayHandle::new(),
        )),
        raw_window_handle: wgpu::rwh::RawWindowHandle::Win32(win32),
    };
    unsafe { instance.create_surface_unsafe(surface_target).ok() }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact values in, exact values out
mod tests {
    use super::*;

    #[test]
    fn host_scale_policy() {
        assert_eq!(host_scale_override(None), None);
        assert_eq!(host_scale_override(Some(0.0)), None);
        assert_eq!(host_scale_override(Some(f64::NAN)), None);
        let expected = if cfg!(target_os = "macos") {
            None
        } else {
            Some(1.5)
        };
        assert_eq!(host_scale_override(Some(1.5)), expected);
    }

    #[test]
    fn host_scale_is_never_overwritten_by_os() {
        let scale = EditorScale::new(1.0);
        scale.set_from_os(2.0);
        assert_eq!(scale.get(), 2.0);
        assert_eq!(scale.host_override(), None);

        scale.set_from_host(1.25);
        assert_eq!(scale.get(), 1.25);
        scale.set_from_os(2.0);
        if cfg!(target_os = "macos") {
            // macOS: the host only seeds the value; the backing scale wins.
            assert_eq!(scale.host_override(), None);
            assert_eq!(scale.get(), 2.0);
        } else {
            // One scale source, never host x OS.
            assert_eq!(scale.host_override(), Some(1.25));
            assert_eq!(scale.get(), 1.25);
        }
    }

    #[test]
    fn window_scale_only_while_open_off_macos() {
        let scale = EditorScale::new(1.5);
        assert_eq!(scale.window_scale(false), None);
        let expected = if cfg!(target_os = "macos") {
            None
        } else {
            Some(1.5)
        };
        assert_eq!(scale.window_scale(true), expected);
    }

    #[test]
    fn bad_scales_are_dropped() {
        let scale = EditorScale::new(-1.0);
        assert_eq!(scale.get(), 1.0);
        scale.set_from_host(f64::INFINITY);
        assert_eq!(scale.host_override(), None);
        assert_eq!(scale.get(), 1.0);
    }
}
