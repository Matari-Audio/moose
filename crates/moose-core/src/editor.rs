use std::collections::HashMap;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use moose_params::Params;
use moose_params::sample::Float;

use crate::events::TransportInfo;
use crate::main_thread::{HostNotice, MenuTarget, ParamIndication, ParamIndicationState};
use crate::tasks::{AnyTaskSpawner, TaskSpawner};

/// A lock-free editor factory bound to a plugin's param store.
///
/// `PluginExport::editor_builder` returns one of these at instance
/// creation; format wrappers cache it outside the plugin lock and call
/// it when the host opens the GUI. For a static build the closure builds
/// the editor from the concrete logic type; for a `--shell` build it
/// rebuilds from the currently loaded dylib, so GUI edits hot-reload
/// (picked up on the next editor close+open). `Send + Sync` so wrappers
/// can stash it in their instance struct and call it from the GUI thread.
pub type EditorBuilder<P> = Box<dyn Fn(Arc<P>) -> Option<Box<dyn Editor>> + Send + Sync>;

/// Raw platform window handle for GUI parenting.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum RawWindowHandle {
    AppKit(*mut std::ffi::c_void), // macOS NSView*
    UiKit(*mut std::ffi::c_void),  // iOS / iPadOS UIView*
    Win32(*mut std::ffi::c_void),  // HWND
    X11(u64),                      // X11 Window ID
    /// A host's Wayland surface (VST3 `WaylandSurfaceID`): the editor maps a
    /// subsurface of `surface` on the host's connection. Both pointers stay
    /// valid on the host UI thread from `open` until `close` returns. Only
    /// editors whose [`Editor::supports_wayland_embedding`] is true get one.
    WaylandSurface {
        /// The host's `wl_display*`.
        display: *mut std::ffi::c_void,
        /// The parent `wl_surface*`, a proxy on `display`.
        surface: *mut std::ffi::c_void,
    },
    /// A slot in a host's nested Wayland compositor (CLAP
    /// `wayland-surface-embedding-v1`, a draft): the editor connects on
    /// `socket` and embeds its own surface with `token`. Only editors whose
    /// [`Editor::supports_wayland_embedding`] is true get one.
    WaylandNested {
        /// A socket connected to the host's compositor. `open` takes
        /// ownership: the editor closes it, whether or not it opens.
        socket: i32,
        /// The embedding token, NUL-terminated, valid during `open`.
        token: *const std::ffi::c_char,
    },
}

/// Plugin GUI editor.
pub trait Editor: Send {
    /// Whether this editor provides a UI, independent of its size or open state.
    fn has_ui(&self) -> bool {
        true
    }

    /// Initial window size in logical points.
    ///
    /// On a 2x Retina display, `(400, 300)` produces an 800x600 pixel window.
    /// On a 1x display, it produces a 400x300 pixel window.
    fn size(&self) -> (u32, u32);

    /// Create the GUI as a child of the host-provided parent window.
    fn open(&mut self, parent: RawWindowHandle, context: PluginContext);

    /// Whether the last [`open`](Editor::open) (or floating open) produced a
    /// window that is still open. Wrappers read it right after `open` and
    /// report a failure to the host (CLAP `set_parent` `false`, VST3
    /// `attached` `kResultFalse`, AU view `nil`). Defaults to `true` for
    /// editors that cannot tell.
    fn is_open(&self) -> bool {
        true
    }

    /// Whether this editor supports a floating X11 window.
    fn supports_floating_x11(&self) -> bool {
        false
    }

    /// Open a floating X11 window and signal native close through `closed`.
    fn open_floating_x11(
        &mut self,
        _context: PluginContext,
        _title: &str,
        _closed: Arc<dyn Fn() + Send + Sync>,
    ) -> bool {
        false
    }

    /// Whether this editor implements Linux CLAP floating Wayland windows.
    /// Embedded Wayland surfaces are not part of the CLAP GUI contract.
    fn supports_floating_wayland(&self) -> bool {
        false
    }

    /// Open a floating Wayland toplevel. Called by CLAP show, after create.
    /// `closed` signals a compositor close; it must never call host GUI APIs
    /// directly. The wrapper marshals it to the main thread.
    fn open_floating_wayland(
        &mut self,
        _context: PluginContext,
        _title: &str,
        _closed: Arc<dyn Fn() + Send + Sync>,
    ) -> bool {
        false
    }

    /// Update the native floating title, when supported.
    fn set_floating_title(&mut self, _title: &str) {}

    /// Whether `open` accepts [`RawWindowHandle::WaylandSurface`] and
    /// [`RawWindowHandle::WaylandNested`]. Such an
    /// editor runs no thread of its own: the host drives it through
    /// [`Self::event_fd`], [`Self::on_fd`] and [`Self::on_timer`].
    fn supports_wayland_embedding(&self) -> bool {
        false
    }

    /// While an embedded Wayland editor is open: the fd the host watches on its
    /// UI thread (VST3 `IRunLoop::registerEventHandler`, CLAP
    /// `posix-fd-support`). Readable means call
    /// [`Self::on_fd`]. The fd stays the same until `close`.
    #[cfg(unix)]
    fn event_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    /// [`Self::event_fd`] became readable. Host UI thread.
    fn on_fd(&mut self) {}

    /// Whether [`Self::event_fd`] has buffered outgoing Wayland requests
    /// awaiting writability (`EAGAIN` flush -> `CLAP_POSIX_FD_WRITE`).
    /// Host UI thread.
    fn wants_write(&self) -> bool {
        false
    }

    /// Synchronize or desynchronize an embedded Wayland subsurface around a
    /// host-driven resize (VST3 3.8 `onSize`: `set_sync` before committing the
    /// resized buffer, `set_desync` after). Host UI thread.
    fn set_wayland_sync(&mut self, _sync: bool) {}

    /// Bind a floating Wayland window as transient to a host-exported
    /// `xdg-foreign-v2` handle (`clap.gui-xdg-foreign-v2/1`). Return `true`
    /// when accepted. Host UI thread.
    fn set_wayland_transient(&mut self, _handle: &str) -> bool {
        false
    }

    /// Apply a Wayland `xdg-activation-v1` focus token from the host
    /// (`clap.gui-xdg-activation-v1/1`). Return `true` when accepted. Host UI
    /// thread.
    fn set_wayland_activation_token(&mut self, _token: &str) -> bool {
        false
    }

    /// The host's ~16 ms UI timer (VST3 `IRunLoop::registerTimer`, CLAP
    /// `timer-support`), while an
    /// embedded Wayland editor is open. The host may read the shared display
    /// and queue the editor's events without the fd waking, and frame
    /// deadlines need a clock; this catches both. Host UI thread.
    fn on_timer(&mut self) {}

    /// Destroy the GUI.
    fn close(&mut self);

    /// Called ~60fps on the host's UI thread for repaint/animation.
    fn idle(&mut self) {}

    /// Host requests a resize. Return true to accept.
    fn set_size(&mut self, _width: u32, _height: u32) -> bool {
        false
    }

    /// Whether the plugin supports resizing.
    fn can_resize(&self) -> bool {
        false
    }

    /// Whether the editor permits the standalone window to be
    /// maximized (the WM maximize button / double-click-titlebar
    /// maximize / macOS zoom-and-fullscreen / Windows maximize box).
    ///
    /// Standalone-only: in CLAP / VST3 / AU the host owns the window
    /// frame, so this is ignored there (same as `size_increment`'s
    /// WM-snap note). Subordinate to [`Self::can_resize`] - a
    /// non-resizable editor can never be maximized regardless of this
    /// value, since the standalone pins min == max, which already
    /// blocks it.
    ///
    /// Defaults to `false`: the standalone host removes the maximize
    /// affordance from resizable editors, so the window stays within
    /// the edge-drag bounds the WM already enforces and can't jump past
    /// the editor's [`Self::max_size`] into an unpainted margin around
    /// the clamped surface. Override to `true` for editors that render
    /// correctly at arbitrary size (typically an unbounded `max_size`)
    /// and want the maximize affordance.
    fn can_maximize(&self) -> bool {
        false
    }

    /// Minimum size the editor can render at, in logical points.
    /// Defaults to `(1, 1)`. Wrappers consult this for CLAP's
    /// `gui_get_resize_hints` and VST3's `checkSizeConstraint`.
    /// Ignored when `can_resize()` returns `false`.
    fn min_size(&self) -> (u32, u32) {
        (1, 1)
    }

    /// Maximum size the editor can render at, in logical points.
    /// Defaults to `(u32::MAX, u32::MAX)`. Same wrapper consumers
    /// as `min_size`.
    fn max_size(&self) -> (u32, u32) {
        (u32::MAX, u32::MAX)
    }

    /// Logical-point granularity for interactive resize, or `None`
    /// for free (pixel-precise) resizing. The standalone X11 host
    /// maps this onto WM resize increments (`PResizeInc`) so the
    /// window manager snaps edge-drags to whole cells - the same
    /// mechanism terminal emulators use to snap to character cells.
    /// The snap counts from [`Self::min_size`], which is already
    /// cell-aligned, so every allowed size lands on a boundary.
    /// Ignored when `can_resize()` returns `false`.
    fn size_increment(&self) -> Option<(u32, u32)> {
        None
    }

    /// Aspect-ratio constraint as `(numerator, denominator)`, or
    /// `None` for free resizing. CLAP, VST3, AU v3, standalone, and
    /// LV2 honour this; VST2 / AAX silently ignore. Integer pair
    /// (not `f64`) avoids the Cubase-9 aspect-rounding quirk JUCE
    /// special-cases.
    fn aspect_ratio(&self) -> Option<(u32, u32)> {
        None
    }

    /// Hint that the renderer prefers power-of-two surface sizes
    /// (some GPU-backed editors). Maps onto CLAP's
    /// `clap_gui_resize_hints.preserve_aspect_ratio` /
    /// `aspect_ratio_width` siblings; ignored on formats without
    /// an equivalent.
    fn prefers_pow2(&self) -> bool {
        false
    }

    /// Host notifies the editor of a new content scale factor.
    ///
    /// DPI/scale is a host→plugin concept: on VST3 Windows the host
    /// delivers it via `IPlugViewContentScaleSupport`; on CLAP via
    /// `clap_plugin_gui::set_scale`; on macOS/Cocoa `AppKit` handles
    /// Retina backing automatically and hosts typically never call
    /// this at all. Editors that need to size off-screen buffers in
    /// physical pixels should react here, not by exposing a pull-style
    /// `scale_factor()` method that format wrappers were tempted to
    /// multiply `size()` by (which caused double-scaling on macOS VST3).
    fn set_scale_factor(&mut self, _factor: f64) {}

    /// The scale the open editor window renders at, where hosts size the
    /// plugin view in physical pixels (Windows, Linux).
    ///
    /// Wrappers use it to convert between the host's physical pixels and
    /// [`size`](Editor::size) only while the host has not announced a
    /// content scale, so the reported size matches the child window the
    /// editor actually created (e.g. at the monitor DPI or `Xft.dpi`).
    /// Must be `None` on macOS (logical `AppKit` coordinates), when closed,
    /// or when unknown.
    fn window_scale(&self) -> Option<f64> {
        None
    }

    /// No-op, kept for source compatibility. Editors follow the host's
    /// content scale when it sends one and the OS scale otherwise, so the
    /// standalone no longer needs to opt in.
    fn set_uses_system_scale(&mut self, _yes: bool) {}

    /// Plugin state was restored (preset recall, undo, session load).
    ///
    /// Called after `load_state()` while the editor is open. Re-read any
    /// cached state from the plugin. Parameter values are already updated
    /// and will be picked up on the next render - this is only needed for
    /// custom state stored outside the parameter system.
    fn state_changed(&mut self) {}

    /// Parameter ID under logical editor coordinates `(x, y)` (top-left
    /// origin), for host "learn by mouse position" (VST3 `IParameterFinder`).
    /// Returns `None` when no parameter control is at `(x, y)` or the editor
    /// does not expose hit-testing.
    fn param_at(&self, _x: i32, _y: i32) -> Option<u32> {
        None
    }

    /// Render a headless screenshot of the editor at its natural size.
    ///
    /// `params` is a type-erased default-state instance the caller
    /// constructs from the plugin's `Params` type. Backends use it to
    /// build a synthetic `PluginContext` / render context so the
    /// screenshot reflects parameter defaults without needing a live
    /// host.
    ///
    /// Returns `(rgba_pixels, physical_width, physical_height)` - RGBA8
    /// row-major, ready to feed into `moose_test::assert_screenshot_pixels`.
    /// Default impl returns `None`; backends that support headless
    /// capture (built-in widgets, egui, iced, slint) override.
    ///
    /// Used by `moose_test::assert_screenshot::<Plugin>(...)` for one-line
    /// snapshot regression tests. Editors backed by frameworks that
    /// don't expose a headless render path (e.g. raw-window-handle
    /// users wiring their own Metal/OpenGL) keep the default `None`.
    fn screenshot(&mut self, params: Arc<dyn moose_params::Params>) -> Option<(Vec<u8>, u32, u32)> {
        let _ = params;
        None
    }
}

/// Bind an editor to the exact managed-task generation that built it.
///
/// Hot-reload shells construct the editor and clone the current fixed task
/// bundle while holding one loader lock, then return this wrapper. Every
/// format may still attach its cached task handle to [`PluginContext`], but
/// [`Editor::open`] replaces it here with the generation-matched bundle.
/// An editor from an older dylib can therefore never enqueue a changed task
/// type into the current dylib's lane after a reload.
#[must_use]
pub fn bind_editor_tasks(
    editor: Box<dyn Editor>,
    tasks: Option<AnyTaskSpawner>,
) -> Box<dyn Editor> {
    Box::new(TaskBoundEditor { editor, tasks })
}

struct TaskBoundEditor {
    editor: Box<dyn Editor>,
    tasks: Option<AnyTaskSpawner>,
}

impl Editor for TaskBoundEditor {
    fn has_ui(&self) -> bool {
        self.editor.has_ui()
    }

    fn size(&self) -> (u32, u32) {
        self.editor.size()
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        self.editor
            .open(parent, context.with_tasks(self.tasks.clone()));
    }

    fn is_open(&self) -> bool {
        self.editor.is_open()
    }

    fn supports_floating_x11(&self) -> bool {
        self.editor.supports_floating_x11()
    }
    fn open_floating_x11(
        &mut self,
        context: PluginContext,
        title: &str,
        closed: Arc<dyn Fn() + Send + Sync>,
    ) -> bool {
        self.editor
            .open_floating_x11(context.with_tasks(self.tasks.clone()), title, closed)
    }
    fn supports_floating_wayland(&self) -> bool {
        self.editor.supports_floating_wayland()
    }
    fn open_floating_wayland(
        &mut self,
        context: PluginContext,
        title: &str,
        closed: Arc<dyn Fn() + Send + Sync>,
    ) -> bool {
        self.editor
            .open_floating_wayland(context.with_tasks(self.tasks.clone()), title, closed)
    }
    fn set_floating_title(&mut self, title: &str) {
        self.editor.set_floating_title(title);
    }
    fn supports_wayland_embedding(&self) -> bool {
        self.editor.supports_wayland_embedding()
    }
    #[cfg(unix)]
    fn event_fd(&self) -> Option<std::os::fd::RawFd> {
        self.editor.event_fd()
    }
    fn on_fd(&mut self) {
        self.editor.on_fd();
    }
    fn wants_write(&self) -> bool {
        self.editor.wants_write()
    }
    fn set_wayland_sync(&mut self, sync: bool) {
        self.editor.set_wayland_sync(sync);
    }
    fn set_wayland_transient(&mut self, handle: &str) -> bool {
        self.editor.set_wayland_transient(handle)
    }
    fn set_wayland_activation_token(&mut self, token: &str) -> bool {
        self.editor.set_wayland_activation_token(token)
    }
    fn on_timer(&mut self) {
        self.editor.on_timer();
    }

    fn close(&mut self) {
        self.editor.close();
    }

    fn idle(&mut self) {
        self.editor.idle();
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        self.editor.set_size(width, height)
    }

    fn can_resize(&self) -> bool {
        self.editor.can_resize()
    }

    fn can_maximize(&self) -> bool {
        self.editor.can_maximize()
    }

    fn min_size(&self) -> (u32, u32) {
        self.editor.min_size()
    }

    fn max_size(&self) -> (u32, u32) {
        self.editor.max_size()
    }

    fn size_increment(&self) -> Option<(u32, u32)> {
        self.editor.size_increment()
    }

    fn aspect_ratio(&self) -> Option<(u32, u32)> {
        self.editor.aspect_ratio()
    }

    fn prefers_pow2(&self) -> bool {
        self.editor.prefers_pow2()
    }

    fn set_scale_factor(&mut self, factor: f64) {
        self.editor.set_scale_factor(factor);
    }

    fn window_scale(&self) -> Option<f64> {
        self.editor.window_scale()
    }

    fn set_uses_system_scale(&mut self, yes: bool) {
        self.editor.set_uses_system_scale(yes);
    }

    fn state_changed(&mut self) {
        self.editor.state_changed();
    }

    fn param_at(&self, x: i32, y: i32) -> Option<u32> {
        self.editor.param_at(x, y)
    }

    fn screenshot(&mut self, params: Arc<dyn Params>) -> Option<(Vec<u8>, u32, u32)> {
        self.editor.screenshot(params)
    }
}

/// Fluent terminal for `editor()` impls: box any concrete editor into
/// the `Box<dyn Editor>` the trait returns, dropping the `Box::new(…)`
/// wrapper.
///
/// ```ignore
/// fn editor(params: Arc<MyParams>) -> Box<dyn Editor> {
///     MuiEditor::new(params, ui, (W, H), build).into_editor()
/// }
/// ```
///
/// Implemented for every [`Editor`] via a blanket impl and re-exported
/// from every `moose::prelude*`, so it's in scope without an extra
/// import.
pub trait IntoEditor {
    /// Box this editor into a `Box<dyn Editor>`.
    fn into_editor(self) -> Box<dyn Editor>;
}

impl<E: Editor + 'static> IntoEditor for E {
    fn into_editor(self) -> Box<dyn Editor> {
        Box::new(self)
    }
}

/// No-op editor for headless plugins that provide no custom GUI.
///
/// Reports a `(0, 0)` size and `is_open() == false`, so hosts fall back to
/// their generic parameter view and standalone runs headless.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEditor;

impl Editor for NoEditor {
    fn has_ui(&self) -> bool {
        false
    }

    fn size(&self) -> (u32, u32) {
        (0, 0)
    }

    fn open(&mut self, _parent: RawWindowHandle, _context: PluginContext) {}

    fn is_open(&self) -> bool {
        false
    }

    fn close(&mut self) {}
}

/// Bridge between the editor and the host / plugin. Format wrappers
/// (CLAP / VST3 / VST2 / AU / AAX / LV2) implement this trait - or
/// build a [`ClosureBridge`] from per-method closures - and pass an
/// `Arc<dyn EditorBridge>` to the editor through [`PluginContext`].
///
/// Editors call into the bridge for everything they can't do
/// directly: starting / ending an automation gesture, reading or
/// writing parameters in normalized or plain form, requesting a
/// window resize, exchanging custom state, sampling the host's
/// transport. Implementations carry whatever per-format pointers
/// the work needs (`clap_host*`, `AEffect*`, an `Arc<P>` for the
/// param store, etc.).
///
/// `Send + Sync` is required so editors can clone the
/// `Arc<dyn EditorBridge>` and hand it to UI worker threads or
/// background animation timers without forcing every implementor to
/// rederive thread-safety bounds.
pub trait EditorBridge: Send + Sync {
    /// Start an automation gesture for `id`. Hosts that show "touched"
    /// state in the automation lane use this to render the
    /// in-progress edit.
    fn begin_edit(&self, id: u32);
    /// Set parameter `id` to `normalized` (clamped to `0.0..=1.0`).
    /// Format wrappers usually plumb this through both the plugin's
    /// own param store and the host's automation channel.
    fn set_param(&self, id: u32, normalized: f64);
    /// End the automation gesture started by [`Self::begin_edit`].
    fn end_edit(&self, id: u32);
    /// Ask the host to resize the editor window to `(w, h)` logical
    /// points. `true` means the request was made, or queued for the host's
    /// UI thread when this is not it (VST3); `false` that the host refused
    /// it or has no window to resize. Either way the size is only settled
    /// when the host calls the editor's `set_size`.
    fn request_resize(&self, w: u32, h: u32) -> bool;
    /// Read the parameter's current normalized value from the plugin
    /// (host→GUI sync path).
    fn get_param(&self, id: u32) -> f64;
    /// Read the parameter's current plain (denormalized) value.
    fn get_param_plain(&self, id: u32) -> f64;
    /// Format the parameter's current value as a display string,
    /// applying the plugin's `format_value` impl + unit suffix.
    fn format_param(&self, id: u32) -> String;
    /// Format into a caller-provided buffer instead of returning a
    /// fresh `String`. The default impl calls
    /// [`Self::format_param`] and copies, so the *bridge-internal*
    /// allocation still happens; the win for the caller is that the
    /// `out` buffer's capacity is reused across calls (e.g.
    /// `ParamCache::sync` polls one label per changed param per
    /// frame and would otherwise drop+reallocate the cached
    /// `String` slot every time). Bridges that produce the formatted
    /// string from raw value bytes can override to drop the
    /// internal allocation too.
    fn format_param_into(&self, id: u32, out: &mut String) {
        out.clear();
        out.push_str(&self.format_param(id));
    }
    /// Read a meter value (0.0–1.0) by meter ID. Returns 0.0 if the
    /// meter ID isn't registered.
    fn get_meter(&self, id: u32) -> f32;
    /// Read the plugin's custom state (everything outside the
    /// parameter system). Returns an empty `Vec` when the plugin has
    /// no custom state.
    fn get_state(&self) -> Vec<u8>;
    /// Write custom state back to the plugin (calls `load_state()`).
    fn set_state(&self, data: Vec<u8>);
    /// Most-recently-reported host transport state, or `None` if the
    /// host does not expose transport to plugin editors or the plugin
    /// has not yet received a process block.
    ///
    /// Format wrappers populate a shared [`TransportSlot`](crate::TransportSlot)
    /// from their process callback; this method reads from it.
    fn transport(&self) -> Option<TransportInfo>;
}

/// Adapter that implements [`EditorBridge`] over per-method closures.
///
/// Format wrappers that prefer to compose state inline via closures
/// construct one of these and wrap it in an `Arc<dyn EditorBridge>`.
/// Wrappers that already have a typed host-pointer struct should
/// `impl EditorBridge` for that struct directly and skip this
/// adapter; one less layer of indirection per call.
pub struct ClosureBridge {
    pub begin_edit: Box<dyn Fn(u32) + Send + Sync>,
    pub set_param: Box<dyn Fn(u32, f64) + Send + Sync>,
    pub end_edit: Box<dyn Fn(u32) + Send + Sync>,
    pub request_resize: Box<dyn Fn(u32, u32) -> bool + Send + Sync>,
    pub get_param: Box<dyn Fn(u32) -> f64 + Send + Sync>,
    pub get_param_plain: Box<dyn Fn(u32) -> f64 + Send + Sync>,
    pub format_param: Box<dyn Fn(u32) -> String + Send + Sync>,
    pub get_meter: Box<dyn Fn(u32) -> f32 + Send + Sync>,
    pub get_state: Box<dyn Fn() -> Vec<u8> + Send + Sync>,
    pub set_state: Box<dyn Fn(Vec<u8>) + Send + Sync>,
    pub transport: Box<dyn Fn() -> Option<TransportInfo> + Send + Sync>,
}

impl EditorBridge for ClosureBridge {
    fn begin_edit(&self, id: u32) {
        (self.begin_edit)(id);
    }
    fn set_param(&self, id: u32, normalized: f64) {
        (self.set_param)(id, normalized);
    }
    fn end_edit(&self, id: u32) {
        (self.end_edit)(id);
    }
    fn request_resize(&self, w: u32, h: u32) -> bool {
        (self.request_resize)(w, h)
    }
    fn get_param(&self, id: u32) -> f64 {
        (self.get_param)(id)
    }
    fn get_param_plain(&self, id: u32) -> f64 {
        (self.get_param_plain)(id)
    }
    fn format_param(&self, id: u32) -> String {
        (self.format_param)(id)
    }
    fn get_meter(&self, id: u32) -> f32 {
        (self.get_meter)(id)
    }
    fn get_state(&self) -> Vec<u8> {
        (self.get_state)()
    }
    fn set_state(&self, data: Vec<u8>) {
        (self.set_state)(data);
    }
    fn transport(&self) -> Option<TransportInfo> {
        (self.transport)()
    }
}

/// Which editor notifications the format already makes safe from any thread.
/// Reads and `set_state` are plugin-local operations; these flags describe only
/// automation gestures and host window resize requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorNotificationDelivery {
    /// The editor must marshal gestures and resize requests to its opening thread.
    #[default]
    HostThread,
    /// Gestures already use the format's thread-safe queue (CLAP). Resize still
    /// requires the host thread. Do not put another queue in front of gestures.
    QueuedEdits,
    /// Both gestures and resize are thread-safe (standalone).
    QueuedAll,
}

/// Live host modulation state for one parameter (plain units).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ParamModulation {
    /// Monophonic modulation offset (`CLAP_EVENT_PARAM_MOD` with `note_id == -1`).
    pub mono: f64,
    /// Polyphonic modulation envelope `(min, max)` observed across active notes
    /// when the host has sent per-note `CLAP_EVENT_PARAM_MOD` (`note_id >= 0`).
    pub poly_range: Option<(f64, f64)>,
}

struct PolyModSlot {
    id: u32,
    /// Packed `(min: f32, max: f32)` in a single atomic word:
    /// high 32 bits = `min.to_bits()`, low 32 bits = `max.to_bits()`.
    /// [`Self::EMPTY`] (`u64::MAX`, a quiet-NaN pair) denotes an inactive slot
    /// so `(0.0, 0.0)` stays distinct from inactive, and a reader sees both
    /// bounds from one load (no torn reads, no reader retry).
    packed: AtomicU64,
}

impl PolyModSlot {
    const EMPTY: u64 = u64::MAX;

    fn new(id: u32) -> Self {
        Self {
            id,
            packed: AtomicU64::new(Self::EMPTY),
        }
    }

    #[inline]
    fn pack(min: f32, max: f32) -> u64 {
        ((u64::from(min.to_bits())) << 32) | u64::from(max.to_bits())
    }

    #[inline]
    fn unpack(bits: u64) -> Option<(f32, f32)> {
        if bits == Self::EMPTY {
            return None;
        }
        #[allow(clippy::cast_possible_truncation)]
        let min = f32::from_bits((bits >> 32) as u32);
        #[allow(clippy::cast_possible_truncation)]
        let max = f32::from_bits(bits as u32);
        if !min.is_finite() || !max.is_finite() {
            return None;
        }
        Some((min.min(max), min.max(max)))
    }

    #[inline]
    fn record(&self, amount: f64) -> bool {
        if !amount.is_finite() {
            return false;
        }
        // A finite offset beyond the f32 range saturates to `±f32::MAX`
        // so it still reports active modulation.
        #[allow(clippy::cast_possible_truncation)]
        let value = (amount as f32).clamp(f32::MIN, f32::MAX);
        let mut cur = self.packed.load(Ordering::Acquire);
        loop {
            let (min, max) = match Self::unpack(cur) {
                Some((lo, hi)) => (lo.min(value), hi.max(value)),
                None => (value, value),
            };
            let next = Self::pack(min, max);
            match self
                .packed
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    #[inline]
    fn clear(&self) {
        self.packed.store(Self::EMPTY, Ordering::Release);
    }

    #[inline]
    fn read(&self) -> Option<(f64, f64)> {
        Self::unpack(self.packed.load(Ordering::Acquire))
            .map(|(min, max)| (f64::from(min), f64::from(max)))
    }
}

/// Per-instance reactive UI feedback shared between a format wrapper and its
/// open editor:
///
/// - Main-thread host [`ParamIndicationState`] table (`clap.param-indication/4`)
///   with a monotonic revision counter.
/// - Pre-allocated, lock-free single-`AtomicU64` per-parameter polyphonic
///   modulation range slots (updated on the audio thread with zero
///   allocations or locks; recording is a lock-free `compare_exchange_weak`
///   retry loop, which can retry spuriously, and reading is a single load).
/// - A coalesced editor wakeup callback invoked on the host main thread when
///   indications, automation, or modulation change.
pub struct EditorFeedback {
    indications: RwLock<HashMap<u32, ParamIndicationState>>,
    indication_rev: AtomicU64,
    poly_slots: Box<[PolyModSlot]>,
    any_poly_active: AtomicBool,
    has_waker: AtomicBool,
    waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl std::fmt::Debug for EditorFeedback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorFeedback")
            .field(
                "indication_rev",
                &self.indication_rev.load(Ordering::Relaxed),
            )
            .field("poly_slot_count", &self.poly_slots.len())
            .finish_non_exhaustive()
    }
}

impl EditorFeedback {
    /// Allocate feedback storage for the given parameter IDs. Pre-allocates the
    /// sorted poly-modulation slot array once off the audio thread so audio
    /// updates never allocate or lock.
    #[must_use]
    pub fn new(param_ids: impl IntoIterator<Item = u32>) -> Self {
        let mut ids: Vec<u32> = param_ids.into_iter().collect();
        ids.sort_unstable();
        ids.dedup();
        let poly_slots = ids
            .into_iter()
            .map(PolyModSlot::new)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            indications: RwLock::new(HashMap::new()),
            indication_rev: AtomicU64::new(0),
            poly_slots,
            any_poly_active: AtomicBool::new(false),
            has_waker: AtomicBool::new(false),
            waker: Mutex::new(None),
        }
    }

    /// Main thread: apply a host [`ParamIndication`] update and wake the open
    /// editor when the indication changed.
    pub fn set_indication(&self, id: u32, indication: &ParamIndication) -> bool {
        let changed = {
            let mut map = self
                .indications
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.entry(id).or_default().apply(indication)
        };
        if changed {
            self.indication_rev.fetch_add(1, Ordering::Release);
            self.wake_editor();
        }
        changed
    }

    /// Read the current combined [`ParamIndicationState`] for `id`.
    #[must_use]
    pub fn indication(&self, id: u32) -> ParamIndicationState {
        self.indications
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }

    /// Monotonic revision bumped on every indication change.
    #[must_use]
    pub fn indication_revision(&self) -> u64 {
        self.indication_rev.load(Ordering::Acquire)
    }

    /// Audio thread (RT-safe: zero allocations, zero locks): reset active
    /// polyphonic modulation slots before recording a new block's events so
    /// each slot reflects active live modulation instead of latching session
    /// extremes forever.
    #[inline]
    pub fn begin_poly_mod_block(&self) {
        let _ = self.clear_poly_mod();
    }

    /// Audio thread (RT-safe: zero allocations, zero locks): record a
    /// polyphonic `CLAP_EVENT_PARAM_MOD` offset for `id`. NaN and infinite
    /// offsets are ignored; finite offsets beyond the `f32` range saturate to
    /// `±f32::MAX`. Uses a lock-free CAS retry loop (see [`EditorFeedback`]).
    #[inline]
    pub fn record_poly_mod(&self, id: u32, amount: f64) {
        if let Ok(idx) = self.poly_slots.binary_search_by_key(&id, |s| s.id)
            && self.poly_slots[idx].record(amount)
        {
            self.any_poly_active.store(true, Ordering::Release);
        }
    }

    /// Audio/main thread (RT-safe: zero allocations, zero locks): clear all
    /// polyphonic modulation summaries (e.g. on reset/deactivate/stop/silence).
    /// Returns `true` when any slot was active and cleared.
    #[inline]
    pub fn clear_poly_mod(&self) -> bool {
        if !self.any_poly_active.load(Ordering::Acquire) {
            return false;
        }
        if !self.any_poly_active.swap(false, Ordering::AcqRel) {
            return false;
        }
        for slot in &self.poly_slots {
            slot.clear();
        }
        true
    }

    /// Read the current polyphonic modulation range `(min, max)` for `id`.
    #[must_use]
    pub fn poly_range(&self, id: u32) -> Option<(f64, f64)> {
        self.poly_slots
            .binary_search_by_key(&id, |s| s.id)
            .ok()
            .and_then(|idx| self.poly_slots[idx].read())
    }

    /// Build the combined [`ParamModulation`] snapshot for `id` using `mono`
    /// from the parameter store plus the tracked polyphonic range.
    #[must_use]
    pub fn modulation(&self, id: u32, mono: f64) -> ParamModulation {
        ParamModulation {
            mono,
            poly_range: self.poly_range(id),
        }
    }

    /// Audio-thread safe (single atomic load) check for whether an open editor
    /// has registered a wakeup callback. Format wrappers check this before
    /// signalling `HostSignal::EDITOR_WAKE` so headless/closed-editor
    /// automation never wakes the host main thread unnecessarily.
    #[inline]
    #[must_use]
    pub fn has_waker(&self) -> bool {
        self.has_waker.load(Ordering::Acquire)
    }

    /// Install or clear the editor's 0 Hz render-loop wakeup callback.
    pub fn set_waker(&self, waker: Option<Arc<dyn Fn() + Send + Sync>>) {
        let mut slot = self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.has_waker.store(waker.is_some(), Ordering::Release);
        *slot = waker;
    }

    /// Main thread: invoke the registered editor waker (if an editor is open).
    /// Releases the internal lock before calling the callback.
    pub fn wake_editor(&self) {
        let waker = self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(wake) = waker {
            wake();
        }
    }
}

/// Everything an editor asks of its host, implemented once per format
/// wrapper and handed to the editor inside its [`PluginContext`].
///
/// The [`EditorBridge`] supertrait carries the parameter gestures, reads,
/// custom state and resize request. The methods here are the host
/// notifications a format may or may not have; each defaults to "this
/// format has no such call". Every method may be called from any thread the
/// editor runs on: a wrapper whose host API is main-thread-only queues the
/// call on its [`HostQueue`](crate::host_queue::HostQueue) and makes it in
/// its main-thread callback.
pub trait HostContext: EditorBridge {
    /// Which of the bridge's notifications are already safe off the host's
    /// UI thread. Metadata only.
    fn delivery(&self) -> EditorNotificationDelivery {
        EditorNotificationDelivery::HostThread
    }

    /// The project changed outside the parameters (CLAP
    /// `state.mark_dirty`, VST3 `IComponentHandler2::setDirty`).
    fn mark_dirty(&self) {}

    /// Open or close a group of edits the host treats as one undo step
    /// (VST3 `IComponentHandler2::startGroupEdit` / `finishGroupEdit`,
    /// CLAP `clap.undo/4`).
    fn group_edits(&self, _edge: EditGroup) {}

    /// Inform the host which parameter control is currently hovered by the
    /// pointer (`None` when no control is hovered; CLAP `clap.param-hovered/1`).
    fn param_hovered(&self, _id: Option<u32>) {}

    /// Ask the host to perform an undo step (CLAP `clap.undo/4`).
    fn request_undo(&self) {}

    /// Ask the host to perform a redo step (CLAP `clap.undo/4`).
    fn request_redo(&self) {}

    /// Ask the host to deactivate and reactivate the plugin, for a change
    /// the processor can only pick up in `activate` (CLAP
    /// `host.request_restart`, VST3 `restartComponent`).
    fn request_restart(&self) {}

    /// Show the host's context menu for `target` at `(x, y)`, logical
    /// points from the editor's top-left corner (CLAP `context-menu.popup`,
    /// VST3 `IComponentHandler3::createContextMenu`). The plugin's own items
    /// come from [`MainThreadHooks::menu_items`](crate::main_thread::MainThreadHooks::menu_items).
    /// `true` when the request was made or queued; `false` when the format
    /// or host has no host menu, and the editor should show its own.
    fn context_menu(&self, _target: MenuTarget, _x: i32, _y: i32) -> bool {
        false
    }

    /// The plugin's note names, voice info or ports changed.
    fn notify(&self, _notice: HostNotice) {}

    /// The editor's window moved to another OS scale. See
    /// [`PluginContext::window_rescaled`].
    fn window_rescaled(&self, _scale: f64) {}

    /// A key the editor's own top-level window didn't use; `true` when the
    /// host took it. See [`PluginContext::unhandled_key`].
    fn unhandled_key(&self, _key: &keyboard_types::KeyboardEvent) -> bool {
        false
    }

    /// Forward an unhandled Wayland XKB key event to the host
    /// (`clap.gui-xkb-key-forwarding/1`). Any thread.
    fn forward_xkb_key(
        &self,
        _keycode: u32,
        _keysym: u32,
        _modifiers: u32,
        _pressed: bool,
    ) -> bool {
        false
    }

    /// Current host indication state (automation state/colour and controller
    /// mapping) for parameter `id` (`clap.param-indication/4`).
    fn param_indication(&self, _id: u32) -> ParamIndicationState {
        ParamIndicationState::default()
    }

    /// Monotonic revision counter bumped whenever any parameter's
    /// [`ParamIndicationState`] changes.
    fn param_indication_revision(&self) -> u64 {
        0
    }

    /// Current live modulation offsets (mono + optional poly range, in plain
    /// units) for parameter `id` (`CLAP_EVENT_PARAM_MOD`).
    fn param_modulation(&self, _id: u32) -> ParamModulation {
        ParamModulation::default()
    }

    /// Register or clear a main-thread wakeup callback that nudges the open
    /// editor's 0 Hz render loop when host automation, modulation, or
    /// indications change.
    fn set_editor_waker(&self, _waker: Option<Arc<dyn Fn() + Send + Sync>>) {}
}

/// A [`ClosureBridge`] is a host with no optional notifications.
impl HostContext for ClosureBridge {}

/// A bare [`EditorBridge`] seen as a [`HostContext`] with no optional
/// notifications.
struct BridgeHost(Arc<dyn EditorBridge>);

impl EditorBridge for BridgeHost {
    fn begin_edit(&self, id: u32) {
        self.0.begin_edit(id);
    }
    fn set_param(&self, id: u32, normalized: f64) {
        self.0.set_param(id, normalized);
    }
    fn end_edit(&self, id: u32) {
        self.0.end_edit(id);
    }
    fn request_resize(&self, w: u32, h: u32) -> bool {
        self.0.request_resize(w, h)
    }
    fn get_param(&self, id: u32) -> f64 {
        self.0.get_param(id)
    }
    fn get_param_plain(&self, id: u32) -> f64 {
        self.0.get_param_plain(id)
    }
    fn format_param(&self, id: u32) -> String {
        self.0.format_param(id)
    }
    fn format_param_into(&self, id: u32, out: &mut String) {
        self.0.format_param_into(id, out);
    }
    fn get_meter(&self, id: u32) -> f32 {
        self.0.get_meter(id)
    }
    fn get_state(&self) -> Vec<u8> {
        self.0.get_state()
    }
    fn set_state(&self, data: Vec<u8>) {
        self.0.set_state(data);
    }
    fn transport(&self) -> Option<TransportInfo> {
        self.0.transport()
    }
}

impl HostContext for BridgeHost {}

/// Context passed to [`Editor::open`]: the wrapper's [`HostContext`], the
/// background-task lanes and the typed parameter store.
///
/// Plugin authors `Deref` to `&P` and read fields directly:
/// `state.gain.read()`. The default `P = dyn Params` keeps the
/// trait-object boundary (`Editor::open(ctx: PluginContext)`) one-typed;
/// editor crates that want typed access (moose-egui, moose-slint,
/// moose-iced) carry their own `<P>` and reconstitute `PluginContext<P>`
/// internally via [`PluginContext::with_params`] using the `Arc<P>` they
/// stored at editor construction.
///
/// `Clone` is a few refcount bumps. Editors that need to hand the context
/// to UI worker threads or animation timers clone freely.
pub struct PluginContext<P: ?Sized = dyn Params> {
    host: Arc<dyn HostContext>,
    /// The bridge gestures, reads and state go through: the host itself
    /// unless an editor-side adapter replaced it ([`Self::with_bridge`]).
    bridge: Arc<dyn EditorBridge>,
    delivery: EditorNotificationDelivery,
    /// Background-task spawner bundle (one lane per declared task type),
    /// stamped by the format wrapper from `PluginExport::task_spawner`
    /// when the plugin wired `tasks:`. Lets the editor schedule work via
    /// [`Self::tasks`]. `None` for a plugin with no background tasks.
    tasks: Option<AnyTaskSpawner>,
    params: Arc<P>,
}

/// One edge of a group of parameter edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EditGroup {
    Begin,
    End,
}

impl<P: ?Sized> Clone for PluginContext<P> {
    fn clone(&self) -> Self {
        Self {
            host: Arc::clone(&self.host),
            bridge: Arc::clone(&self.bridge),
            delivery: self.delivery,
            tasks: self.tasks.clone(),
            params: Arc::clone(&self.params),
        }
    }
}

impl<P: ?Sized> PluginContext<P> {
    /// Build a context from the wrapper's [`HostContext`] and the plugin's
    /// typed param store. Add background-task scheduling with
    /// [`Self::with_tasks`].
    pub fn from_host(host: Arc<dyn HostContext>, params: Arc<P>) -> Self {
        let delivery = host.delivery();
        Self {
            bridge: Arc::clone(&host) as Arc<dyn EditorBridge>,
            host,
            delivery,
            tasks: None,
            params,
        }
    }

    /// Build a context from a bare [`EditorBridge`]: a host with none of
    /// the optional notifications. Tests and editor harnesses use this;
    /// format wrappers implement [`HostContext`] and call
    /// [`Self::from_host`].
    pub fn new(bridge: Arc<dyn EditorBridge>, params: Arc<P>) -> Self {
        Self::from_host(Arc::new(BridgeHost(bridge)), params)
    }

    /// Tell the host the project changed in a way it cannot see through
    /// parameters (custom state edited in the editor), so it offers to save.
    /// Parameter edits need no call: the host tracks those itself. A no-op
    /// on formats without the API (AU, standalone). Any thread.
    pub fn mark_dirty(&self) {
        self.host.mark_dirty();
    }

    /// The open window moved to another OS scale (Windows monitor DPI, X11
    /// `Xft.dpi`) and kept its logical size. Call it before asking the host
    /// for that size with [`Self::request_resize`], which then converts at
    /// the new scale. Not for host-set scales. Any thread.
    pub fn window_rescaled(&self, scale: f64) {
        self.host.window_rescaled(scale);
    }

    /// Offer the host a key the editor's top-level window didn't use;
    /// true when the host took it. Embedded editors never call it: their
    /// host's window gets unused keys by itself. Any thread.
    #[must_use]
    pub fn unhandled_key(&self, key: &keyboard_types::KeyboardEvent) -> bool {
        self.host.unhandled_key(key)
    }

    /// Forward an unhandled XKB key event from an embedded/nested Wayland
    /// editor to the host (`clap.gui-xkb-key-forwarding/1`). Returns `true`
    /// when queued for host delivery. Any thread.
    #[must_use]
    pub fn forward_xkb_key(
        &self,
        keycode: u32,
        keysym: u32,
        modifiers: u32,
        pressed: bool,
    ) -> bool {
        self.host
            .forward_xkb_key(keycode, keysym, modifiers, pressed)
    }

    /// Start a group of edits the host treats as one (one undo step), such
    /// as a preset morph that moves several parameters. Close it with
    /// [`Self::end_group`]; the gestures inside still need
    /// `begin_edit` / `end_edit`. A no-op on formats without the API. Any
    /// thread.
    pub fn begin_group(&self) {
        self.host.group_edits(EditGroup::Begin);
    }

    /// Close the group started by [`Self::begin_group`].
    pub fn end_group(&self) {
        self.host.group_edits(EditGroup::End);
    }

    /// Report the parameter control currently under the pointer (`None` when
    /// none is hovered). Forwarded to CLAP `clap.param-hovered/1` when enabled.
    /// Any thread.
    pub fn param_hovered(&self, id: Option<u32>) {
        self.host.param_hovered(id);
    }

    /// Ask the host to undo the last step (CLAP `clap.undo/4`). Any thread.
    pub fn request_undo(&self) {
        self.host.request_undo();
    }

    /// Ask the host to redo the last undone step (CLAP `clap.undo/4`). Any thread.
    pub fn request_redo(&self) {
        self.host.request_redo();
    }

    /// Ask the host to deactivate and reactivate the plugin, for a change
    /// the processor only picks up in `activate` (an oversampling switch
    /// that changes latency). A no-op where the format has no such call.
    /// Any thread.
    pub fn request_restart(&self) {
        self.host.request_restart();
    }

    /// Show the host's context menu for `target` (a parameter's automation
    /// and MIDI-learn entries, plus the plugin's own
    /// [`MainThreadHooks::menu_items`](crate::main_thread::MainThreadHooks::menu_items))
    /// at `(x, y)`, logical points from the editor's top-left corner.
    /// `false` when the host has no such menu; show the editor's own then.
    /// Any thread.
    #[must_use]
    pub fn context_menu(&self, target: MenuTarget, x: i32, y: i32) -> bool {
        self.host.context_menu(target, x, y)
    }

    /// Tell the host `notice` changed (CLAP `note_name.changed`,
    /// `voice_info.changed`). A no-op on formats without the API. Any
    /// thread except the audio thread.
    pub fn notify_host(&self, notice: HostNotice) {
        self.host.notify(notice);
    }

    /// Current host indication state (automation state/colour and mapped
    /// controller metadata) for parameter `id`.
    #[must_use]
    pub fn param_indication(&self, id: impl Into<u32>) -> ParamIndicationState {
        self.host.param_indication(id.into())
    }

    /// Monotonic revision counter for host parameter indications.
    #[must_use]
    pub fn param_indication_revision(&self) -> u64 {
        self.host.param_indication_revision()
    }

    /// Live modulation offsets (mono + optional poly range, plain units) for
    /// parameter `id`.
    #[must_use]
    pub fn param_modulation(&self, id: impl Into<u32>) -> ParamModulation {
        self.host.param_modulation(id.into())
    }

    /// Register (`Some`) or clear (`None`) a wakeup callback that wakes the
    /// editor's 0 Hz render loop when host automation, modulation, or
    /// parameter indications change.
    pub fn set_editor_waker(&self, waker: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.host.set_editor_waker(waker);
    }

    /// Which notifications the format already queues safely.
    #[must_use]
    pub fn notification_delivery(&self) -> EditorNotificationDelivery {
        self.delivery
    }

    /// Override the notification delivery reported by
    /// [`Self::notification_delivery`] (for test harnesses built with
    /// [`Self::new`] or [`Self::from_closures`]).
    #[must_use]
    pub fn with_notification_delivery(mut self, delivery: EditorNotificationDelivery) -> Self {
        self.delivery = delivery;
        self
    }

    /// Install an editor-side bridge adapter without losing task lanes, the
    /// typed parameter store, or the host's notifications.
    #[must_use]
    pub fn with_bridge(mut self, bridge: Arc<dyn EditorBridge>) -> Self {
        self.bridge = bridge;
        self
    }

    /// Attach the background-task spawner (from
    /// `PluginExport::task_spawner`). Format wrappers call this when
    /// building the editor context.
    #[must_use]
    pub fn with_tasks(mut self, tasks: Option<AnyTaskSpawner>) -> Self {
        self.tasks = tasks;
        self
    }

    /// The background-task spawner for task type `T`, or `None` if the
    /// plugin declared no `tasks:` lane of that type. Scheduling with it is
    /// wait-free, so it is safe from the GUI thread.
    #[must_use]
    pub fn tasks<T: Send + 'static>(&self) -> Option<TaskSpawner<T>> {
        self.tasks.as_ref().and_then(AnyTaskSpawner::downcast::<T>)
    }

    /// Access the underlying bridge handle. Editors that want to clone
    /// the bridge into a worker thread without cloning the surrounding
    /// `PluginContext` use this.
    #[must_use]
    pub fn bridge(&self) -> &Arc<dyn EditorBridge> {
        &self.bridge
    }

    /// The wrapper's host handle, for editors that pass it to a worker.
    #[must_use]
    pub fn host(&self) -> &Arc<dyn HostContext> {
        &self.host
    }

    /// Access the typed param store as an `Arc`. Use this when you
    /// need to capture the params in a `'static` closure (e.g. an iced
    /// `Subscription` or a worker thread).
    #[must_use]
    pub fn params(&self) -> &Arc<P> {
        &self.params
    }

    /// Replace the param-store generic parameter while reusing the
    /// same host and bridge. Used by editor crates that receive the
    /// dyn-erased `PluginContext` from [`Editor::open`] and want the typed
    /// `PluginContext<P>` for their UI closure.
    pub fn with_params<Q: ?Sized>(&self, params: Arc<Q>) -> PluginContext<Q> {
        PluginContext {
            host: Arc::clone(&self.host),
            bridge: Arc::clone(&self.bridge),
            delivery: self.delivery,
            tasks: self.tasks.clone(),
            params,
        }
    }

    pub fn begin_edit(&self, id: impl Into<u32>) {
        self.bridge.begin_edit(id.into());
    }
    pub fn set_param(&self, id: impl Into<u32>, normalized: f64) {
        self.bridge.set_param(id.into(), normalized);
    }
    pub fn end_edit(&self, id: impl Into<u32>) {
        self.bridge.end_edit(id.into());
    }
    /// Begin + set + end in one call. Use for click-to-toggle widgets
    /// and similar single-shot edits where the gesture and the value
    /// arrive together.
    pub fn automate(&self, id: impl Into<u32>, normalized: f64) {
        let id = id.into();
        self.bridge.begin_edit(id);
        self.bridge.set_param(id, normalized);
        self.bridge.end_edit(id);
    }
    /// Ask the host to resize the editor to `(w, h)` logical points.
    /// `true` means the request was made, or queued for the host's UI thread
    /// when called from another thread (VST3); the size is settled only when
    /// the host calls [`Editor::set_size`].
    ///
    /// **Re-entrancy:** call this from your editor's own event / render loop,
    /// not synchronously from inside an [`Editor`] method the host drives
    /// (`open`, `set_size`, `size`, `state_changed`, `close`). Several
    /// wrappers hold an internal editor lock across those calls, and on some
    /// formats `request_resize` reaches back into it - directly (AU) or via a
    /// host that answers `resizeView` synchronously (VST3). Such a re-entrant
    /// call is deferred (applied on a later frame) rather than applied inline,
    /// so a "correct my aspect ratio from within `set_size`" pattern won't
    /// take effect immediately; do that shaping by returning the adjusted size
    /// or requesting from the next frame instead.
    #[must_use]
    pub fn request_resize(&self, w: u32, h: u32) -> bool {
        self.bridge.request_resize(w, h)
    }
    pub fn format_param(&self, id: impl Into<u32>) -> String {
        self.bridge.format_param(id.into())
    }
    /// Format into a caller-owned buffer. See
    /// [`EditorBridge::format_param_into`] for the allocation
    /// trade-off - the caller's buffer is reused, but bridges that
    /// don't override the default impl still allocate internally.
    pub fn format_param_into(&self, id: impl Into<u32>, out: &mut String) {
        self.bridge.format_param_into(id.into(), out);
    }
    pub fn get_meter(&self, id: impl Into<u32>) -> f32 {
        self.bridge.get_meter(id.into())
    }
    #[must_use]
    pub fn get_state(&self) -> Vec<u8> {
        self.bridge.get_state()
    }
    /// Hand custom state to the plugin (its `load_state`) and mark the
    /// project dirty, since the host cannot see this change otherwise.
    pub fn set_state(&self, data: Vec<u8>) {
        self.bridge.set_state(data);
        self.mark_dirty();
    }
    #[must_use]
    pub fn transport(&self) -> Option<TransportInfo> {
        self.bridge.transport()
    }
}

impl PluginContext<dyn Params> {
    /// Build a dyn-erased context from a [`ClosureBridge`]: a host with no
    /// optional notifications. Tests and editor harnesses use this.
    pub fn from_closures(bridge: ClosureBridge, params: Arc<dyn Params>) -> Self {
        Self::from_host(Arc::new(bridge), params)
    }
}

impl<P: Params + 'static> PluginContext<P> {
    /// Drop the typed `<P>` and return the dyn-erased context that
    /// crosses the `Editor::open` trait-object boundary.
    #[must_use]
    pub fn dyn_erase(self) -> PluginContext<dyn Params> {
        PluginContext {
            host: self.host,
            bridge: self.bridge,
            delivery: self.delivery,
            tasks: self.tasks,
            params: self.params as Arc<dyn Params>,
        }
    }
}

/// Plugin authors read parameter fields directly via `Deref`:
/// `state.gain.read()`, `state.bypass.value()`. The `state`
/// here is `&PluginContext<MyParams>` and `Deref::Target = MyParams`.
impl<P: ?Sized> Deref for PluginContext<P> {
    type Target = P;
    fn deref(&self) -> &P {
        &self.params
    }
}

/// Build a [`PluginContext`] backed only by `params`. All write
/// closures are no-ops; reads delegate to the params `Arc`; the
/// transport reports the deterministic
/// [`crate::events::TransportInfo::for_screenshot`] state so
/// screenshot tests stay reproducible across CI runs.
///
/// Used by editor backends inside their `Editor::screenshot()` impl,
/// and re-exported from `moose-test` for plugin authors that want to
/// drive snapshot tests directly.
pub fn for_test_params(params: Arc<dyn Params>) -> PluginContext<dyn Params> {
    let p_get = Arc::clone(&params);
    let p_plain = Arc::clone(&params);
    let p_fmt = Arc::clone(&params);
    let transport = TransportInfo::for_screenshot();
    PluginContext::from_closures(
        ClosureBridge {
            begin_edit: Box::new(|_| {}),
            set_param: Box::new(|_, _| {}),
            end_edit: Box::new(|_| {}),
            request_resize: Box::new(|_, _| false),
            get_param: Box::new(move |id| p_get.get_normalized(id).unwrap_or(0.5)),
            get_param_plain: Box::new(move |id| p_plain.get_plain(id).unwrap_or(0.0)),
            format_param: Box::new(move |id| {
                let plain = p_fmt.get_plain(id).unwrap_or(0.0);
                p_fmt
                    .format_value(id, plain)
                    .unwrap_or_else(|| format!("{plain:.2}"))
            }),
            get_meter: Box::new(|_| 0.0),
            get_state: Box::new(Vec::new),
            set_state: Box::new(|_| {}),
            transport: Box::new(move || Some(transport)),
        },
        params,
    )
}

// ---------------------------------------------------------------------------
// Precision-routed parameter reads
//
// The editor-bridge surface is sample-agnostic (`f64` on the wire, the
// lossless lowest-common-denominator that round-trips any host
// automation precision). These two extension traits route the call
// site to the user's chosen precision - same pattern as
// `FloatParamReadF32` / `FloatParamReadF64` for the audio-thread
// param reads. Brought into scope via `pub use ... as _;` in each
// prelude:
//   - `prelude` / `prelude32`        → `PluginContextReadF32`
//   - `prelude64` / `prelude64m`     → `PluginContextReadF64`
//
// Single-prelude code dispatches unambiguously. Importing both
// preludes in the same file collides on `get_param` - the right
// error if the file hasn't committed to a precision.
// ---------------------------------------------------------------------------

/// `f32`-precision parameter reads on `PluginContext`. Brought into
/// scope by `moose::prelude` / `moose::prelude32` / `moose::prelude64m`
/// (the `f32`-buffer preludes). GUI binding crates (slint, egui,
/// iced) take `f32` natively, so this is the common case.
pub trait PluginContextReadF32 {
    /// Normalized `[0, 1]` value of the parameter, narrowed to `f32`.
    fn get_param(&self, id: impl Into<u32>) -> f32;
    /// Plain (denormalized) value of the parameter, narrowed to `f32`.
    fn get_param_plain(&self, id: impl Into<u32>) -> f32;
}

/// `f64`-precision parameter reads on `PluginContext`. Brought into
/// scope by `moose::prelude64`. Same surface as
/// [`PluginContextReadF32`] but returns the bridge's `f64` value
/// directly without narrowing.
pub trait PluginContextReadF64 {
    /// Normalized `[0, 1]` value of the parameter.
    fn get_param(&self, id: impl Into<u32>) -> f64;
    /// Plain (denormalized) value of the parameter.
    fn get_param_plain(&self, id: impl Into<u32>) -> f64;
}

impl<P: ?Sized> PluginContextReadF32 for PluginContext<P> {
    fn get_param(&self, id: impl Into<u32>) -> f32 {
        self.bridge.get_param(id.into()).to_f32()
    }
    fn get_param_plain(&self, id: impl Into<u32>) -> f32 {
        self.bridge.get_param_plain(id.into()).to_f32()
    }
}

impl<P: ?Sized> PluginContextReadF64 for PluginContext<P> {
    fn get_param(&self, id: impl Into<u32>) -> f64 {
        self.bridge.get_param(id.into())
    }
    fn get_param_plain(&self, id: impl Into<u32>) -> f64 {
        self.bridge.get_param_plain(id.into())
    }
}

/// Constrain a host-requested logical size to an editor's
/// [`Editor::min_size`] / [`Editor::max_size`] / [`Editor::aspect_ratio`].
/// Shared by every format wrapper so they enforce identical constraints.
///
/// With an aspect ratio set, fit the *largest on-ratio rectangle that fits
/// inside* the requested box: derive height from width and keep it if it
/// fits, otherwise the width is the limiting axis and height drives it. The
/// result is `<=` the request on both axes, so the editor surface never
/// exceeds the host window and can never clip - whatever odd size a host
/// hands us (some skip an aspect pre-flight and pass raw drag dimensions),
/// the worst case is an on-ratio letterbox inside the window. The rule is a
/// pure function of `(w, h)` - no "which edge moved" guess - so a drag can't
/// make the chosen axis flip and judder. `u64` arithmetic for the
/// multiplication so a hypothetical `(u32::MAX, 1)` aspect doesn't overflow
/// before the clamp lands.
#[must_use]
pub fn fit_logical_size(w: u32, h: u32, editor: &dyn Editor) -> (u32, u32) {
    fit_size(
        w,
        h,
        editor.min_size(),
        editor.max_size(),
        editor.aspect_ratio(),
    )
}

/// Same fit as [`fit_logical_size`] but over raw constraints rather than
/// an `&dyn Editor`. Lets call sites that have already captured the
/// bounds (e.g. an Objective-C resize callback that can't carry a trait
/// object) reuse the identical rule.
#[must_use]
pub fn fit_size(
    w: u32,
    h: u32,
    min: (u32, u32),
    max: (u32, u32),
    aspect: Option<(u32, u32)>,
) -> (u32, u32) {
    let (min_w, min_h) = min;
    let (max_w, max_h) = max;
    let mut w = w.clamp(min_w.max(1), max_w);
    let mut h = h.clamp(min_h.max(1), max_h);
    if let Some((num64, denom64)) = ratio64(aspect) {
        // The on-ratio height for this width. Unclamped so the comparison
        // sees the true ratio, not a bound-pinned value.
        let h_from_w = u64::from(w) * denom64 / num64;
        if h_from_w <= u64::from(h) {
            // Width is the limiting axis: shrink height onto the ratio.
            h = derive_height(w, min_h, max_h, num64, denom64).0;
        } else {
            // Height is the limiting axis: shrink width onto the ratio.
            w = derive_width(h, min_w, max_w, num64, denom64).0;
        }
    }
    (w, h)
}

/// Clamp a host-committed size to the editor's `[min, max]` bounds only,
/// leaving the aspect ratio untouched so the editor fills the host window
/// exactly. The commit-time counterpart to [`fit_logical_size`]: on-ratio
/// shaping already happened during the host's drag negotiation (a preflight
/// such as VST3 `checkSizeConstraint`), so re-fitting onto the ratio here
/// would only floor the editor a pixel under the window and leave an
/// unpainted letterbox line. The `max` clamp keeps the surface inside the
/// window; the `min` clamp upholds the editor's "can't render smaller than
/// this" floor even when a host hands over a too-small box.
#[must_use]
pub fn clamp_logical_size(w: u32, h: u32, editor: &dyn Editor) -> (u32, u32) {
    let (min_w, min_h) = editor.min_size();
    let (max_w, max_h) = editor.max_size();
    (w.clamp(min_w.max(1), max_w), h.clamp(min_h.max(1), max_h))
}

/// Enforces size constraints on host resizes that bypassed the format's
/// negotiation hooks. Some hosts resize the plugin's embedded window
/// directly at the windowing-system level (Bitwig on Linux/X11 resizes
/// the embed window itself), so no `checkSizeConstraint`-style preflight
/// ever runs - the editor's own `Resized` handler is the last place that
/// can enforce `min_size` / `max_size` / `aspect_ratio`.
///
/// [`Self::fit`] returns the size the editor should render at, plus an
/// optional corrective size to push back to the host
/// (`PluginContext::request_resize`). Each offending host size triggers at
/// most one correction, and the corrective size itself satisfies the
/// constraints, so a host that refuses (or echoes) the request can't be
/// spun into a resize feedback loop.
#[derive(Default)]
pub struct ResizeCorrector {
    /// Whether we've already pushed a corrective resize back to the host
    /// for the current out-of-bounds excursion. Latches on the first
    /// push-back and clears only when the host hands us an in-bounds size
    /// again. A host that bypasses negotiation and then ignores the
    /// push-back would otherwise be re-asked every frame and spun into a
    /// runaway resize loop: Bitwig on Linux returns success to
    /// `request_resize` but instead *grows* the embed window a few px per
    /// call, and jitters the size on the un-clamped axis so the fitted
    /// target changes every frame. Latching on "have we asked since the
    /// last in-bounds size" - rather than on the requested size - sends
    /// exactly one request per excursion even when that target wobbles.
    pushed_back: bool,
}

impl ResizeCorrector {
    /// Fit a host-driven logical size against the constraints. Returns
    /// the fitted size to render at and, when the host size was out of
    /// bounds and we haven't already pushed back this excursion, the size
    /// to request back.
    pub fn fit(
        &mut self,
        w: u32,
        h: u32,
        min: (u32, u32),
        max: (u32, u32),
        aspect: Option<(u32, u32)>,
    ) -> ((u32, u32), Option<(u32, u32)>) {
        let fitted = fit_size(w, h, min, max, aspect);
        if fitted == (w, h) {
            // In-bounds: the host is cooperating (or the drag returned
            // within bounds); re-arm the next excursion's one-shot push.
            self.pushed_back = false;
            return (fitted, None);
        }
        // Out of bounds: push back exactly once per excursion.
        let request = (!self.pushed_back).then_some(fitted);
        self.pushed_back = true;
        (fitted, request)
    }
}

/// A usable `(num, denom)` ratio as `u64`, or `None` when no aspect is set
/// or either term is zero. `u64` so the on-ratio multiplications below can't
/// overflow before the clamp lands (a hypothetical `(u32::MAX, 1)` aspect).
fn ratio64(aspect: Option<(u32, u32)>) -> Option<(u64, u64)> {
    match aspect {
        Some((num, denom)) if num > 0 && denom > 0 => Some((u64::from(num), u64::from(denom))),
        _ => None,
    }
}

/// On-ratio height for `w`, clamped into `[min_h, max_h]`. The flag reports
/// whether the clamp moved the value (i.e. a bound was hit), so the caller
/// knows whether the source axis needs re-deriving to stay on-ratio.
#[allow(clippy::cast_possible_truncation)]
fn derive_height(w: u32, min_h: u32, max_h: u32, num64: u64, denom64: u64) -> (u32, bool) {
    let on_ratio = (u64::from(w) * denom64 / num64).clamp(1, u64::from(u32::MAX)) as u32;
    let clamped = on_ratio.clamp(min_h.max(1), max_h);
    (clamped, clamped != on_ratio)
}

/// On-ratio width for `h`, clamped into `[min_w, max_w]`. Flag as in
/// [`derive_height`].
#[allow(clippy::cast_possible_truncation)]
fn derive_width(h: u32, min_w: u32, max_w: u32, num64: u64, denom64: u64) -> (u32, bool) {
    let on_ratio = (u64::from(h) * num64 / denom64).clamp(1, u64::from(u32::MAX)) as u32;
    let clamped = on_ratio.clamp(min_w.max(1), max_w);
    (clamped, clamped != on_ratio)
}

#[cfg(test)]
mod corrector_tests {
    use super::ResizeCorrector;

    const MIN: (u32, u32) = (300, 200);
    const MAX: (u32, u32) = (900, 600);

    #[test]
    fn in_bounds_size_passes_through_without_correction() {
        let mut c = ResizeCorrector::default();
        assert_eq!(c.fit(400, 300, MIN, MAX, None), ((400, 300), None));
    }

    #[test]
    fn out_of_bounds_pushes_once_per_excursion() {
        let mut c = ResizeCorrector::default();
        // First out-of-bounds sight: fit + one push-back.
        let (fitted, req) = c.fit(1200, 800, MIN, MAX, None);
        assert_eq!(fitted, (900, 600));
        assert_eq!(req, Some((900, 600)));
        // Host refused / echoed the same size: no repeat request.
        assert_eq!(c.fit(1200, 800, MIN, MAX, None), ((900, 600), None));
        // Host ignores it and keeps feeding out-of-bounds sizes - crucially,
        // even ones whose *fitted target wobbles* (a different clamp on the
        // un-pinned axis each frame). We stay quiet, so a host that grows in
        // response to each request (Bitwig) can't be spun into a runaway.
        assert_eq!(c.fit(1300, 590, MIN, MAX, None), ((900, 590), None));
        assert_eq!(c.fit(1300, 595, MIN, MAX, None), ((900, 595), None));
        // ...even a swing to the opposite bound stays quiet mid-excursion.
        assert_eq!(c.fit(100, 100, MIN, MAX, None), ((300, 200), None));
        // Host finally hands us an in-bounds size: excursion over, re-arm.
        assert_eq!(c.fit(800, 500, MIN, MAX, None), ((800, 500), None));
        // Next excursion gets a fresh single push-back.
        let (_, req) = c.fit(1200, 800, MIN, MAX, None);
        assert_eq!(req, Some((900, 600)));
    }

    #[test]
    fn honoured_correction_resets_the_guard() {
        let mut c = ResizeCorrector::default();
        let _ = c.fit(1200, 800, MIN, MAX, None);
        // Host applied the corrective size: in bounds, guard resets...
        assert_eq!(c.fit(900, 600, MIN, MAX, None), ((900, 600), None));
        // ...so the same offending size requests again next time.
        let (_, req) = c.fit(1200, 800, MIN, MAX, None);
        assert_eq!(req, Some((900, 600)));
    }

    #[test]
    fn aspect_violation_corrects_onto_ratio() {
        let mut c = ResizeCorrector::default();
        let ((w, h), req) = c.fit(800, 600, MIN, MAX, Some((4, 3)));
        assert_eq!((w, h), (800, 600), "already on-ratio passes through");
        assert_eq!(req, None);
        let ((w, h), req) = c.fit(800, 400, MIN, MAX, Some((4, 3)));
        assert_eq!((w, h), (533, 400), "height-limited fit onto 4:3");
        assert_eq!(req, Some((533, 400)));
    }
}

#[cfg(test)]
mod fit_tests {
    use super::{Editor, PluginContext, RawWindowHandle, fit_logical_size};

    /// Minimal editor stub: only the bounds/aspect hooks
    /// `fit_logical_size` reads carry meaning; the rest are unused.
    struct StubEditor {
        min: (u32, u32),
        max: (u32, u32),
        aspect: Option<(u32, u32)>,
    }

    impl Editor for StubEditor {
        fn size(&self) -> (u32, u32) {
            self.min
        }
        fn open(&mut self, _parent: RawWindowHandle, _context: PluginContext) {}
        fn close(&mut self) {}
        fn min_size(&self) -> (u32, u32) {
            self.min
        }
        fn max_size(&self) -> (u32, u32) {
            self.max
        }
        fn aspect_ratio(&self) -> Option<(u32, u32)> {
            self.aspect
        }
    }

    fn stub(aspect: Option<(u32, u32)>) -> StubEditor {
        StubEditor {
            min: (320, 240),
            max: (u32::MAX, u32::MAX),
            aspect,
        }
    }

    #[test]
    fn no_aspect_clamps_each_axis_to_bounds() {
        let e = stub(None);
        assert_eq!(fit_logical_size(800, 600, &e), (800, 600));
        assert_eq!(fit_logical_size(100, 100, &e), (320, 240));
    }

    #[test]
    fn tall_box_is_width_bound() {
        // A box taller than 4:3 fits the full width; height shrinks onto
        // the ratio so the result never overflows the box.
        let e = stub(Some((4, 3)));
        assert_eq!(fit_logical_size(640, 800, &e), (640, 480));
    }

    #[test]
    fn wide_box_is_height_bound() {
        // A box wider than 4:3 fits the full height; width shrinks instead.
        let e = stub(Some((4, 3)));
        assert_eq!(fit_logical_size(800, 480, &e), (640, 480));
    }

    #[test]
    fn on_ratio_box_is_unchanged() {
        let e = stub(Some((4, 3)));
        assert_eq!(fit_logical_size(800, 600, &e), (800, 600));
    }

    #[test]
    fn fit_never_exceeds_the_requested_box() {
        // The no-clip invariant: for any box at or above `min_size`, the
        // aspect fit stays inside it on both axes, so the editor surface
        // can never overflow the host window. `min` is on the 16:9 ratio so
        // the fit also stays exactly on-ratio right down to the corner.
        let e = StubEditor {
            min: (320, 180),
            max: (u32::MAX, u32::MAX),
            aspect: Some((16, 9)),
        };
        for &(w, h) in &[
            (640, 800),
            (800, 480),
            (1000, 1000),
            (321, 900),
            (1920, 300),
        ] {
            let (rw, rh) = fit_logical_size(w, h, &e);
            assert!(rw <= w && rh <= h, "{rw}x{rh} exceeds box {w}x{h}");
            assert!((i64::from(rw) * 9 - i64::from(rh) * 16).abs() <= 16);
            assert!(rw >= 320 && rh >= 180);
        }
    }
}

#[cfg(test)]
mod host_context_tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn bridge(set_calls: Arc<AtomicUsize>) -> ClosureBridge {
        ClosureBridge {
            begin_edit: Box::new(|_| {}),
            set_param: Box::new(move |_, _| {
                set_calls.fetch_add(1, Ordering::Relaxed);
            }),
            end_edit: Box::new(|_| {}),
            request_resize: Box::new(|_, _| true),
            get_param: Box::new(|_| 0.0),
            get_param_plain: Box::new(|_| 0.0),
            format_param: Box::new(|_| String::new()),
            get_meter: Box::new(|_| 0.0),
            get_state: Box::new(Vec::new),
            set_state: Box::new(|_| {}),
            transport: Box::new(|| None),
        }
    }

    /// A host that records every optional notification.
    struct Recorder {
        bridge: ClosureBridge,
        log: Mutex<Vec<String>>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                bridge: bridge(Arc::new(AtomicUsize::new(0))),
                log: Mutex::new(Vec::new()),
            }
        }
        fn push(&self, entry: String) {
            self.log.lock().expect("log lock").push(entry);
        }
    }

    impl EditorBridge for Recorder {
        fn begin_edit(&self, id: u32) {
            self.push(format!("begin {id}"));
        }
        fn set_param(&self, id: u32, normalized: f64) {
            self.push(format!("set {id} {normalized}"));
        }
        fn end_edit(&self, id: u32) {
            self.push(format!("end {id}"));
        }
        fn request_resize(&self, w: u32, h: u32) -> bool {
            self.bridge.request_resize(w, h)
        }
        fn get_param(&self, id: u32) -> f64 {
            self.bridge.get_param(id)
        }
        fn get_param_plain(&self, id: u32) -> f64 {
            self.bridge.get_param_plain(id)
        }
        fn format_param(&self, id: u32) -> String {
            self.bridge.format_param(id)
        }
        fn get_meter(&self, id: u32) -> f32 {
            self.bridge.get_meter(id)
        }
        fn get_state(&self) -> Vec<u8> {
            self.bridge.get_state()
        }
        fn set_state(&self, data: Vec<u8>) {
            self.push(format!("state {data:?}"));
        }
        fn transport(&self) -> Option<TransportInfo> {
            None
        }
    }

    impl HostContext for Recorder {
        fn delivery(&self) -> EditorNotificationDelivery {
            EditorNotificationDelivery::QueuedEdits
        }
        fn mark_dirty(&self) {
            self.push("dirty".into());
        }
        fn group_edits(&self, edge: EditGroup) {
            self.push(format!("group {edge:?}"));
        }
        fn request_restart(&self) {
            self.push("restart".into());
        }
        fn context_menu(&self, target: MenuTarget, x: i32, y: i32) -> bool {
            self.push(format!("menu {target:?} {x} {y}"));
            true
        }
        fn notify(&self, notice: HostNotice) {
            self.push(format!("notify {notice:?}"));
        }
        fn window_rescaled(&self, scale: f64) {
            self.push(format!("scale {scale}"));
        }
    }

    #[test]
    fn every_editor_call_reaches_the_one_host() {
        let host = Arc::new(Recorder::new());
        let context =
            PluginContext::from_host(Arc::clone(&host) as Arc<dyn HostContext>, Arc::new(()))
                .with_params(Arc::new(()));
        context.begin_group();
        context.automate(3_u32, 0.5);
        context.end_group();
        context.set_state(vec![9]);
        context.request_restart();
        assert!(context.context_menu(MenuTarget::Param(3), 10, 20));
        context.notify_host(HostNotice::VOICE_INFO);
        context.window_rescaled(2.0);
        assert_eq!(
            context.notification_delivery(),
            EditorNotificationDelivery::QueuedEdits
        );
        let log = host.log.lock().expect("log lock").clone();
        assert_eq!(
            log,
            [
                "group Begin",
                "begin 3",
                "set 3 0.5",
                "end 3",
                "group End",
                "state [9]",
                "dirty",
                "restart",
                "menu Param(3) 10 20",
                "notify HostNotice(VOICE_INFO)",
                "scale 2",
            ]
        );
    }

    #[test]
    fn a_bridge_adapter_keeps_the_hosts_notifications_params_and_tasks() {
        let calls = Arc::new(AtomicUsize::new(0));
        let host = Arc::new(Recorder::new());
        let tasks = TaskSpawner::<u8>::new(|_| {});
        let params = Arc::new(());
        let context =
            PluginContext::from_host(host.clone() as Arc<dyn HostContext>, Arc::clone(&params))
                .with_tasks(Some(AnyTaskSpawner::new(&tasks)));
        let adapted = context
            .clone()
            .with_params(Arc::clone(&params))
            .with_bridge(Arc::new(bridge(Arc::clone(&calls))));
        assert_eq!(
            adapted.notification_delivery(),
            EditorNotificationDelivery::QueuedEdits
        );
        assert!(Arc::ptr_eq(adapted.params(), &params));
        assert!(adapted.tasks::<u8>().is_some());
        adapted.set_param(1_u32, 0.5);
        adapted.mark_dirty();
        assert_eq!(calls.load(Ordering::Relaxed), 1, "edits go to the adapter");
        assert_eq!(*host.log.lock().expect("log lock"), ["dirty"]);
    }

    #[test]
    fn a_bare_bridge_has_no_optional_notifications() {
        let context = PluginContext::new(
            Arc::new(bridge(Arc::new(AtomicUsize::new(0)))) as Arc<dyn EditorBridge>,
            Arc::new(()),
        );
        context.mark_dirty();
        context.request_restart();
        assert!(!context.context_menu(MenuTarget::Global, 0, 0));
        assert_eq!(
            context.notification_delivery(),
            EditorNotificationDelivery::HostThread
        );
    }

    #[test]
    fn poly_mod_finite_offset_beyond_f32_range_stays_active() {
        let fb = EditorFeedback::new([7]);
        fb.record_poly_mod(7, 1e40);
        assert_eq!(
            fb.poly_range(7),
            Some((f64::from(f32::MAX), f64::from(f32::MAX)))
        );
        fb.record_poly_mod(7, -1e40);
        assert_eq!(
            fb.poly_range(7),
            Some((f64::from(f32::MIN), f64::from(f32::MAX)))
        );
        fb.begin_poly_mod_block();
        fb.record_poly_mod(7, f64::INFINITY);
        fb.record_poly_mod(7, f64::NAN);
        assert_eq!(fb.poly_range(7), None);
    }

    #[test]
    fn poly_mod_tracks_live_blocks_clears_and_is_atomic_u64_consistent() {
        let feedback = Arc::new(EditorFeedback::new([0, 1]));
        assert_eq!(feedback.poly_range(0), None);
        assert!(!feedback.clear_poly_mod());

        // Zero modulation offset is distinguishable from inactive (`EMPTY`).
        feedback.begin_poly_mod_block();
        feedback.record_poly_mod(0, 0.0);
        assert_eq!(feedback.poly_range(0), Some((0.0, 0.0)));

        // Non-finite values are ignored and cannot corrupt the packed sentinel.
        feedback.record_poly_mod(0, f64::NAN);
        feedback.record_poly_mod(0, f64::INFINITY);
        assert_eq!(feedback.poly_range(0), Some((0.0, 0.0)));

        // Block 1: wide range (-1.0, 3.0).
        feedback.begin_poly_mod_block();
        feedback.record_poly_mod(0, -1.0);
        feedback.record_poly_mod(0, 3.0);
        assert_eq!(feedback.poly_range(0), Some((-1.0, 3.0)));

        // Block 2: narrower live range shrinks instead of latching extremes.
        feedback.begin_poly_mod_block();
        feedback.record_poly_mod(0, 0.25);
        feedback.record_poly_mod(0, 0.5);
        assert_eq!(feedback.poly_range(0), Some((0.25, 0.5)));

        // Clear returns true once, then false when already clear.
        assert!(feedback.clear_poly_mod());
        assert_eq!(feedback.poly_range(0), None);
        assert!(!feedback.clear_poly_mod());

        // Concurrent writer/reader stress test: single-AtomicU64 reader must
        // never observe torn `lo > hi` or uninitialized `(0.0, 0.0)`.
        let done = Arc::new(AtomicBool::new(false));
        let writer_fb = Arc::clone(&feedback);
        let writer_done = Arc::clone(&done);
        let writer = std::thread::spawn(move || {
            for i in 0..5_000u32 {
                writer_fb.begin_poly_mod_block();
                writer_fb.record_poly_mod(0, -2.5);
                writer_fb.record_poly_mod(0, 4.0);
                if i % 3 == 0 {
                    let _ = writer_fb.clear_poly_mod();
                }
            }
            writer_done.store(true, Ordering::Release);
        });

        while !done.load(Ordering::Acquire) {
            if let Some((lo, hi)) = feedback.poly_range(0) {
                assert!(lo <= hi, "torn poly_range: lo={lo} > hi={hi}");
                assert!(
                    lo != 0.0 || hi != 0.0,
                    "observed uninitialized (0.0, 0.0) before min/max store"
                );
            }
        }
        writer.join().unwrap();
    }
}
