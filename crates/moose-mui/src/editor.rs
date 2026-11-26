//! moose's `Editor` over the window in [`crate::window`].
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use moose_core::editor::{Editor, PluginContext, RawWindowHandle};
use moose_params::Params;
use mui::Ui;
use mui::input::Input;
use mui::scene::{El, Size};

use crate::Bridge;
use crate::platform::{HostScale, ParentWindow};
use crate::window::{self, Requests, Shared, View, lock};

#[cfg(target_os = "linux")]
mod embedded;

type Build<P> = Box<dyn FnMut(&mut Ui, &mut Bridge<P>) -> El + Send>;
type Changed = Box<dyn FnMut() -> bool + Send>;
type FileDrop =
    Box<dyn FnMut(&Ui, mui::prelude::Point, &[std::path::PathBuf], bool) -> bool + Send>;
type Cancel = Box<dyn FnMut(&Ui) + Send>;
type Log = Box<dyn FnMut(&str) + Send>;

/// The model half: the app's build closure and the parameters it binds.
pub(crate) struct Session<P: Params> {
    pub(crate) bridge: Bridge<P>,
    build: Build<P>,
    /// [`MuiEditor::changed`]: model state outside the bridge.
    changed: Option<Changed>,
    file_drop: Option<FileDrop>,
    cancel: Option<Cancel>,
    log: Option<Log>,
    closed: Option<Box<dyn FnMut() + Send>>,
    user_zoom: Option<Box<dyn Fn(Size) -> f64 + Send>>,
    /// The size `build` was designed at, fitted to the window; `None` is
    /// [`MuiEditor::fixed_zoom`].
    design: Option<Size>,
    /// Coalescing flag shared with the host editor waker so rapid bursts of
    /// automation/modulation/indication updates poke `Requests::redraw` once
    /// per frame.
    wake_pending: Arc<AtomicBool>,
    /// Last computed zoom factor, shared with [`Bridge`] for logical context
    /// menu coordinates.
    last_zoom: std::sync::atomic::AtomicU64,
}

impl<P: Params> View for Session<P> {
    fn log(&mut self, line: &str) {
        if let Some(log) = &mut self.log {
            log(line);
        } else {
            eprintln!("{line}");
        }
    }
    fn drop_files(
        &mut self,
        ui: &Ui,
        at: mui::prelude::Point,
        paths: &[std::path::PathBuf],
        dropped: bool,
    ) -> bool {
        self.file_drop
            .as_mut()
            .is_some_and(|f| f(ui, at, paths, dropped))
    }
    fn cancel(&mut self, ui: &Ui) {
        if let Some(f) = &mut self.cancel {
            f(ui);
        }
    }
    fn build(&mut self, ui: &mut Ui, input: &Input) -> El {
        // SeqCst pairs with `install_host_waker`'s SeqCst swap so ARM64 cannot
        // reorder subsequent state reads ahead of clearing `wake_pending`.
        self.wake_pending.swap(false, Ordering::SeqCst);
        let zoom = f64::from_bits(self.last_zoom.load(Ordering::Relaxed));
        self.bridge.set_zoom(zoom);
        self.bridge.pointer = input.pointer;
        let root = (self.build)(ui, &mut self.bridge);
        self.bridge.finish_frame(ui, input);
        root
    }
    fn changed(&mut self) -> bool {
        self.wake_pending.swap(false, Ordering::SeqCst);
        // Both run every tick: the bridge snapshots values as it compares.
        self.bridge.changed() | self.changed.as_mut().is_some_and(|f| f())
    }
    fn zoom(&self, window: Size) -> f64 {
        let fit = self.design.map_or(1.0, |d| {
            (window.width / d.width).min(window.height / d.height)
        });
        let user = self.user_zoom.as_ref().map_or(1.0, |f| f(window));
        let zoom = fit
            * if user.is_finite() && user > 0.0 {
                user
            } else {
                1.0
            };
        self.last_zoom.store(zoom.to_bits(), Ordering::Relaxed);
        zoom
    }
    fn request_resize(&mut self, width: u32, height: u32) -> bool {
        self.bridge.request_resize(width, height)
    }
    fn rescaled(&mut self, scale: f64, (width, height): (u32, u32)) {
        if let Some(context) = self.bridge.context() {
            context.window_rescaled(scale);
            // A host that refuses keeps the old pixels; the view still lays
            // out at the new scale, so there's nothing to undo.
            let _ = context.request_resize(width, height);
        }
    }
}

/// A MUI editor for any moose plugin: `build` makes the tree every frame
/// from the retained `Ui` and a [`Bridge`] to the plugin's parameters.
/// State the editor keeps between frames lives in the closure's captures;
/// it and the `Ui` survive a close and reopen.
///
/// The editor redraws when the bridge sees a moose parameter or meter move.
/// Model state outside those -- meters, status, anything in the plugin's own
/// atomics -- must report through [`MuiEditor::changed`], or it only repaints
/// when the mouse moves; `.changed(|| true)` rebuilds every tick.
///
/// A resized window scales the whole tree to fit the `size` it was designed
/// at (`min` of the two axes' ratios, so 1.0 at that size); the spare axis
/// gets the extra room. [`MuiEditor::fixed_zoom`] keeps it at 1.0 instead,
/// so the tree lays out into the new size unscaled.
///
/// ```ignore
/// fn editor(params: Arc<GainParams>) -> Box<dyn Editor> {
///     let ui = Ui::default().font(font);
///     MuiEditor::new(params, ui, (320, 200), |ui, bridge| {
///         bridge.bind(ui, P::Gain, |ui, id, v| knob(ui, id, "Gain", v, 0.0..=1.0))
///     })
///     .resizable((240, 160))
///     // The audio thread sets `dirty` when a meter moves.
///     .changed(move || dirty.swap(false, Ordering::Relaxed))
///     .into_editor()
/// }
/// ```
///
/// # Panics and build profile
///
/// The editor's window callbacks catch panics so a UI bug stays in the UI.
/// That only works when the plugin cdylib unwinds: build it with the
/// workspace's `plugin` profile (`cargo build --profile plugin`), never plain
/// `--release`, whose `panic = "abort"` makes every UI panic abort the DAW.
/// Thread-safe, coalesced wakeup handle for a [`MuiEditor`] window.
///
/// Calling [`EditorWaker::wake`] nudges the currently open window to redraw via
/// [`Requests::redraw`] at most once per frame (coalesced via an atomic flag),
/// and is a cheap no-op while the editor window is closed.
#[derive(Clone, Debug)]
pub struct EditorWaker {
    requests: Arc<Mutex<std::sync::Weak<Requests>>>,
    pending: Arc<AtomicBool>,
}

impl EditorWaker {
    /// Request an immediate redraw of the editor window if one is open and no
    /// wake is already pending.
    pub fn wake(&self) {
        if !self.pending.swap(true, Ordering::SeqCst) {
            let requests = self
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .upgrade();
            if let Some(requests) = requests {
                requests.redraw();
            } else {
                self.pending.store(false, Ordering::Release);
            }
        }
    }
}

pub struct MuiEditor<P: Params> {
    shared: Arc<Mutex<Shared<Session<P>>>>,
    requests: Arc<Requests>,
    active_requests: Arc<Mutex<std::sync::Weak<Requests>>>,
    wake_pending: Arc<AtomicBool>,
    keys: Option<window::KeyHook>,
    win32_parent: Option<Box<dyn FnMut(Option<isize>) + Send>>,
    pointer: Option<window::PointerHook>,
    wheel: window::WheelPolicy,
    poll: Option<std::time::Duration>,
    #[cfg(target_os = "linux")]
    x11_hook: Option<window::X11WindowHook>,
    #[cfg(target_os = "linux")]
    x11_requests: Arc<Mutex<std::sync::Weak<Requests>>>,
    params: Arc<P>,
    size: (u32, u32),
    min: Option<(u32, u32)>,
    pub(crate) scale: HostScale,
    window: Option<Handle>,
    #[cfg(target_os = "linux")]
    floating: Option<window::wayland::FloatingWindow>,
    #[cfg(target_os = "linux")]
    embedded: Option<embedded::Embedded>,
}

/// Whole logical points, as the host's window API takes them.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a window is a few thousand points, never negative"
)]
fn points(s: Size) -> (u32, u32) {
    (
        s.width.round().max(0.0) as u32,
        s.height.round().max(0.0) as u32,
    )
}

#[cfg(target_os = "linux")]
struct Handle(window::baseview::ThreadedWindow);

#[cfg(not(target_os = "linux"))]
struct Handle(window::baseview::Window);

// SAFETY: `baseview::Window` wraps a native window pointer and is not
// auto-`Send`. moose calls `open`, `set_size`, `window_scale` and `close`
// from one GUI thread, never concurrently, so the window never leaves the
// thread that made it; `Send` is only what `Box<dyn Editor>` asks for.
#[cfg(not(target_os = "linux"))]
#[expect(unsafe_code, reason = "vouches Send for the baseview window alone")]
unsafe impl Send for Handle {}

impl<P: Params> MuiEditor<P> {
    /// A fixed-size editor, `size` logical points.
    pub fn new(
        params: Arc<P>,
        ui: Ui,
        size: impl Into<Size>,
        build: impl FnMut(&mut Ui, &mut Bridge<P>) -> El + Send + 'static,
    ) -> Self {
        let size = size.into();
        let wake_pending = Arc::new(AtomicBool::new(false));
        let session = Session {
            bridge: Bridge::new(Arc::clone(&params)),
            build: Box::new(build),
            changed: None,
            file_drop: None,
            cancel: None,
            log: None,
            closed: None,
            user_zoom: None,
            design: Some(size),
            wake_pending: Arc::clone(&wake_pending),
            last_zoom: std::sync::atomic::AtomicU64::new(1.0f64.to_bits()),
        };
        let requests: Arc<Requests> = Arc::default();
        let active_requests = Arc::new(Mutex::new(Arc::downgrade(&requests)));
        Self {
            shared: Arc::new(Mutex::new(Shared { ui, view: session })),
            requests,
            active_requests,
            wake_pending,
            keys: None,
            win32_parent: None,
            pointer: None,
            wheel: window::WheelPolicy::default(),
            poll: None,
            #[cfg(target_os = "linux")]
            x11_hook: None,
            #[cfg(target_os = "linux")]
            x11_requests: Arc::default(),
            params,
            size: points(size),
            min: None,
            scale: HostScale::default(),
            window: None,
            #[cfg(target_os = "linux")]
            floating: None,
            #[cfg(target_os = "linux")]
            embedded: None,
        }
    }

    /// Let the host resize the window, down to `min` logical points.
    #[must_use]
    pub fn resizable(mut self, min: impl Into<Size>) -> Self {
        self.min = Some(points(min.into()));
        self
    }

    /// Poll `f` every display tick alongside the parameters: `true` means
    /// the model moved (a meter, a status) and the tree rebuilds.
    #[must_use]
    pub fn changed(self, f: impl FnMut() -> bool + Send + 'static) -> Self {
        lock(&self.shared).view.changed = Some(Box::new(f));
        self
    }

    /// Receive window and renderer diagnostics.
    #[must_use]
    pub fn on_log(self, f: impl FnMut(&str) + Send + 'static) -> Self {
        lock(&self.shared).view.log = Some(Box::new(f));
        self
    }

    /// Accept file drags and handle their release at a position in scene units.
    #[must_use]
    pub fn on_files(
        self,
        f: impl FnMut(&Ui, mui::prelude::Point, &[std::path::PathBuf], bool) -> bool + Send + 'static,
    ) -> Self {
        lock(&self.shared).view.file_drop = Some(Box::new(f));
        self
    }

    /// Release app-owned gestures when the native host cancels input.
    #[must_use]
    pub fn on_cancel(self, f: impl FnMut(&Ui) + Send + 'static) -> Self {
        lock(&self.shared).view.cancel = Some(Box::new(f));
        self
    }

    /// Inspect key events before MUI routes them; return true to consume one.
    #[must_use]
    pub fn on_key(
        mut self,
        f: impl FnMut(&Ui, &window::KeyEvent) -> bool + Send + 'static,
    ) -> Self {
        let hook: window::KeyHook = Arc::new(Mutex::new(f));
        self.requests.on_key(Arc::clone(&hook));
        self.keys = Some(hook);
        self
    }

    /// Hide the pointer while the retained UI requests it during a gesture.
    #[must_use]
    pub fn hide_pointer(mut self, f: impl FnMut(&Ui) -> bool + Send + 'static) -> Self {
        self.pointer = Some(Arc::new(Mutex::new(f)));
        self
    }

    /// Observe X11 window creation and cancellation on the native thread.
    /// The callback must not borrow the model or wait for its worker.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn on_x11_window(mut self, f: impl FnMut(Option<u32>) + Send + 'static) -> Self {
        self.x11_hook = Some(Arc::new(Mutex::new(f)));
        self
    }

    /// Configure notch conversion for baseview windows.
    #[must_use]
    pub fn wheel_policy(mut self, policy: window::WheelPolicy) -> Self {
        self.wheel = policy;
        self
    }

    /// Poll [`MuiEditor::changed`] at least this often while the window is
    /// open, for model state that moves without waking the editor. A poll
    /// that finds nothing changed builds and draws nothing, unlike a
    /// repaint deadline (`Ui::repaint_in`), which rebuilds the tree each time.
    #[must_use]
    pub fn poll(mut self, interval: std::time::Duration) -> Self {
        self.poll = Some(interval);
        self
    }

    /// Finish app-owned persistence on close and drop.
    #[must_use]
    pub fn on_close(self, f: impl FnMut() + Send + 'static) -> Self {
        lock(&self.shared).view.closed = Some(Box::new(f));
        self
    }

    /// Multiply fit zoom by an app-controlled factor based on logical window size.
    #[must_use]
    pub fn user_zoom(self, f: impl Fn(Size) -> f64 + Send + 'static) -> Self {
        lock(&self.shared).view.user_zoom = Some(Box::new(f));
        self
    }

    /// Observe the host's Win32 owner while the editor is open.
    /// Native dialogs can borrow it through [`ParentWindow::win32`].
    #[must_use]
    pub fn on_win32_parent(mut self, hook: impl FnMut(Option<isize>) + Send + 'static) -> Self {
        self.win32_parent = Some(Box::new(hook));
        self
    }

    fn publish_win32_parent(&mut self, parent: Option<RawWindowHandle>) {
        let owner = match parent {
            Some(RawWindowHandle::Win32(pointer)) if !pointer.is_null() => Some(pointer as isize),
            Some(_) => return,
            None => None,
        };
        if let Some(hook) = &mut self.win32_parent {
            hook(owner);
        }
    }

    fn reset_requests(&mut self) {
        self.requests = Arc::default();
        *self
            .active_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::downgrade(&self.requests);
        if let Some(keys) = &self.keys {
            self.requests.on_key(Arc::clone(keys));
        }
        if let Some(pointer) = &self.pointer {
            self.requests.on_pointer(Arc::clone(pointer));
        }
        self.requests.set_wheel_policy(self.wheel);
        self.requests.set_poll_interval(self.poll);
        self.wake_pending.store(false, Ordering::Release);
        #[cfg(target_os = "linux")]
        if let Some(hook) = &self.x11_hook {
            let hook = Arc::clone(hook);
            let requests = Arc::downgrade(&self.requests);
            *self
                .x11_requests
                .lock()
                .expect("X11 request generation lock poisoned") = requests.clone();
            let current = Arc::clone(&self.x11_requests);
            self.requests.on_x11_window(Arc::new(Mutex::new(move |id| {
                let mut callback = hook.lock().expect("X11 callback lock poisoned");
                if std::sync::Weak::ptr_eq(
                    &requests,
                    &current
                        .lock()
                        .expect("X11 request generation lock poisoned"),
                ) {
                    callback(id);
                }
            })));
        }
    }

    /// Keep the zoom at 1.0 when the window resizes: the tree lays out into
    /// the new size instead of scaling to fit its design size.
    #[must_use]
    pub fn fixed_zoom(self) -> Self {
        lock(&self.shared).view.design = None;
        self
    }

    /// Returns a thread-safe coalesced wakeup handle for this editor.
    ///
    /// Background workers can call [`EditorWaker::wake`] after publishing new
    /// UI state to trigger an immediate redraw without waiting for [`Self::poll`].
    #[must_use]
    pub fn waker(&self) -> EditorWaker {
        EditorWaker {
            requests: Arc::clone(&self.active_requests),
            pending: Arc::clone(&self.wake_pending),
        }
    }

    /// Install a coalesced 0 Hz wakeup callback on the attached host context
    /// so host automation, modulation, and `param-indication` updates nudge
    /// [`Requests::redraw`] without polling.
    fn install_host_waker(&self, context: &PluginContext) {
        let waker = self.waker();
        context.set_editor_waker(Some(Arc::new(move || waker.wake())));
    }

    fn has_window(&self) -> bool {
        #[cfg(target_os = "linux")]
        let native = self.floating.is_some() || self.embedded.is_some();
        #[cfg(not(target_os = "linux"))]
        let native = false;
        self.window.is_some() || native
    }

    /// Physical size and scale of a native Wayland window, floating or embedded.
    #[cfg(target_os = "linux")]
    fn native_wayland_size(&self) -> Option<((u32, u32), f64)> {
        if let Some(window) = &self.floating {
            return Some(window.size());
        }
        self.embedded.as_ref()?.with(|window| window.size())
    }

    /// `open` with a host's Wayland surface: a subsurface the host's loop pumps.
    #[cfg(target_os = "linux")]
    fn embed(&mut self, display: *mut std::ffi::c_void, surface: *mut std::ffi::c_void) {
        let (Some(display), Some(surface)) = (
            std::ptr::NonNull::new(display),
            std::ptr::NonNull::new(surface),
        ) else {
            lock(&self.shared)
                .view
                .log("Wayland: the host gave a null display or surface");
            return;
        };
        lock(&self.shared)
            .view
            .bridge
            .set_native_requests(Arc::downgrade(&self.requests));
        // SAFETY: `RawWindowHandle::WaylandSurface`'s contract: both stay live
        // on this, the host's UI thread, until `close`, which drops the window.
        #[expect(unsafe_code, reason = "the host vouches for its Wayland objects")]
        let opened = unsafe {
            window::wayland::embed(
                display,
                surface,
                self.size,
                Arc::clone(&self.shared),
                Arc::clone(&self.requests),
            )
        };
        self.adopt_embedded(opened);
    }

    /// `open` in a host's nested compositor: our own connection, placed by token.
    #[cfg(target_os = "linux")]
    fn embed_nested(&mut self, socket: i32, token: *const std::ffi::c_char) {
        use std::os::fd::FromRawFd;
        // SAFETY: `RawWindowHandle::WaylandNested`'s contract: `open` owns
        // `socket` (closed on every path below), and `token` is a C string
        // that is live for this call.
        #[expect(unsafe_code, reason = "the host hands over a raw fd and C string")]
        let (socket, token) = unsafe {
            (
                (socket >= 0).then(|| std::os::fd::OwnedFd::from_raw_fd(socket)),
                (!token.is_null()).then(|| std::ffi::CStr::from_ptr(token).to_string_lossy()),
            )
        };
        let (Some(socket), Some(token)) = (socket, token) else {
            lock(&self.shared)
                .view
                .log("Wayland: the host gave no socket or token");
            return;
        };
        lock(&self.shared)
            .view
            .bridge
            .set_native_requests(Arc::downgrade(&self.requests));
        let opened = window::wayland::embed_nested(
            socket,
            &token,
            self.size,
            Arc::clone(&self.shared),
            Arc::clone(&self.requests),
        );
        self.adopt_embedded(opened);
    }

    #[cfg(target_os = "linux")]
    fn adopt_embedded(&mut self, opened: Result<window::wayland::EmbeddedWindow, String>) {
        match opened {
            Ok(window) => self.embedded = Some(embedded::Embedded::adopt(window)),
            Err(error) => lock(&self.shared).view.log(&format!("Wayland: {error}")),
        }
    }

    #[cfg(target_os = "linux")]
    fn pump_embedded(&mut self) {
        embedded::reap_retired();
        let Some(result) = self
            .embedded
            .as_ref()
            .and_then(|e| e.with(window::wayland::EmbeddedWindow::dispatch))
        else {
            return;
        };
        if let Err(error) = result {
            // A Wayland error is fatal to the window; the next timer would only repeat it.
            self.embedded = None;
            lock(&self.shared).view.log(&format!("Wayland: {error}"));
        }
    }

    fn close_window(&mut self) {
        *self
            .active_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = std::sync::Weak::new();
        self.wake_pending.store(false, Ordering::Release);
        #[cfg(target_os = "linux")]
        {
            if self.floating.is_some() || self.embedded.is_some() {
                self.size = self.size();
            }
            drop(self.floating.take());
            drop(self.embedded.take());
            embedded::reap_retired();
        }
        if let Some(Handle(window)) = self.window.take() {
            #[cfg(target_os = "linux")]
            {
                window.close_bounded(std::time::Duration::from_millis(250));
            }
            #[cfg(not(target_os = "linux"))]
            window.close();
        }
    }
}

impl<P: Params> Editor for MuiEditor<P> {
    fn size(&self) -> (u32, u32) {
        #[cfg(target_os = "linux")]
        if let Some(((w, h), scale)) = self.native_wayland_size() {
            return points(Size::new(f64::from(w) / scale, f64::from(h) / scale));
        }
        // The open window, not the last `set_size`: on X11 the window follows
        // its parent without the host calling `set_size`, and a stale answer
        // here makes the host snap the editor back to the old size.
        match self
            .window
            .as_ref()
            .filter(|w| self.min.is_some() && w.0.is_open())
        {
            Some(open) => {
                let l = open.0.size().logical;
                points(Size::new(l.width, l.height))
            }
            None => self.size,
        }
    }

    fn can_resize(&self) -> bool {
        self.min.is_some()
    }

    fn min_size(&self) -> (u32, u32) {
        self.min.unwrap_or(self.size)
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        if self.has_window() {
            self.close();
        }
        // A request made while closed was for the last window.
        self.reset_requests();
        lock(&self.shared)
            .view
            .bridge
            .attach(context.with_params(Arc::clone(&self.params)));
        self.publish_win32_parent(Some(parent));
        #[cfg(target_os = "linux")]
        if let RawWindowHandle::WaylandSurface { display, surface } = parent {
            self.embed(display, surface);
            if self.has_window() {
                self.install_host_waker(&context);
            } else {
                self.close();
            }
            return;
        }
        #[cfg(target_os = "linux")]
        if let RawWindowHandle::WaylandNested { socket, token } = parent {
            self.embed_nested(socket, token);
            if self.has_window() {
                self.install_host_waker(&context);
            } else {
                self.close();
            }
            return;
        }
        #[cfg(target_os = "linux")]
        let opened = if matches!(parent, RawWindowHandle::X11(0)) {
            window::open_floating_threaded(
                "MUI",
                self.size,
                self.scale.policy(),
                Arc::clone(&self.shared),
                Arc::clone(&self.requests),
            )
        } else {
            window::open_threaded(
                &ParentWindow(parent),
                "MUI",
                self.size,
                self.scale.policy(),
                Arc::clone(&self.shared),
                Arc::clone(&self.requests),
            )
        };
        #[cfg(not(target_os = "linux"))]
        let opened = window::open(
            &ParentWindow(parent),
            "MUI",
            self.size,
            self.scale.policy(),
            Arc::clone(&self.shared),
            Arc::clone(&self.requests),
        );
        self.window = opened.map(Handle);
        if self.has_window() {
            self.install_host_waker(&context);
        } else {
            self.close();
        }
    }

    fn is_open(&self) -> bool {
        self.has_window()
    }

    #[cfg(target_os = "linux")]
    fn supports_floating_x11(&self) -> bool {
        true
    }

    #[cfg(target_os = "linux")]
    fn open_floating_x11(
        &mut self,
        context: PluginContext,
        title: &str,
        closed: Arc<dyn Fn() + Send + Sync>,
    ) -> bool {
        self.close();
        self.reset_requests();
        lock(&self.shared)
            .view
            .bridge
            .attach(context.with_params(Arc::clone(&self.params)));
        self.requests.on_close(closed);
        self.window = window::open_floating_threaded(
            title,
            self.size,
            self.scale.policy(),
            Arc::clone(&self.shared),
            Arc::clone(&self.requests),
        )
        .map(Handle);
        if self.window.is_some() {
            self.install_host_waker(&context);
            true
        } else {
            self.close();
            false
        }
    }

    #[cfg(target_os = "linux")]
    fn supports_floating_wayland(&self) -> bool {
        true
    }

    #[cfg(target_os = "linux")]
    fn open_floating_wayland(
        &mut self,
        context: PluginContext,
        title: &str,
        closed: Arc<dyn Fn() + Send + Sync>,
    ) -> bool {
        self.close();
        self.reset_requests();
        lock(&self.shared)
            .view
            .bridge
            .attach(context.with_params(Arc::clone(&self.params)));
        // A floating window has no host window behind it: unused keys go
        // to the host's listener (the standalone's shortcuts and MIDI keys).
        let unhandled = context.clone();
        self.requests
            .on_unhandled_key(Arc::new(move |key| unhandled.unhandled_key(key)));
        lock(&self.shared)
            .view
            .bridge
            .set_native_requests(Arc::downgrade(&self.requests));
        match window::wayland::open(
            title,
            self.size,
            self.min_size(),
            Arc::clone(&self.shared),
            Arc::clone(&self.requests),
            closed,
        ) {
            Ok(window) => {
                self.floating = Some(window);
                self.install_host_waker(&context);
                true
            }
            Err(error) => {
                lock(&self.shared).view.log(&format!("Wayland: {error}"));
                self.close();
                false
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn supports_wayland_embedding(&self) -> bool {
        true
    }

    #[cfg(target_os = "linux")]
    fn event_fd(&self) -> Option<std::os::fd::RawFd> {
        self.embedded.as_ref().map(embedded::Embedded::fd)
    }

    #[cfg(target_os = "linux")]
    fn on_fd(&mut self) {
        self.pump_embedded();
    }

    #[cfg(target_os = "linux")]
    fn on_timer(&mut self) {
        self.pump_embedded();
    }

    #[cfg(target_os = "linux")]
    fn set_floating_title(&mut self, title: &str) {
        if let Some(window) = &self.floating {
            window.title(title);
        }
    }

    fn close(&mut self) {
        self.publish_win32_parent(None);
        {
            // Released before the window closes: on macOS and Windows
            // baseview tears the handler down on this thread, inside
            // `close`, and its last event would wait on this lock.
            let mut s = lock(&self.shared);
            s.view.bridge.close();
            // The bridge ended the host's gestures; these are the same edges.
            s.ui.close();
            let s = &mut *s;
            s.view.cancel(&s.ui);
            if let Some(f) = &mut s.view.closed {
                f();
            }
        }
        self.close_window();
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        match self.min {
            Some((w, h)) if width >= w && height >= h => {
                self.size = (width, height);
                self.requests.resize(width, height);
                true
            }
            _ => false,
        }
    }

    fn set_scale_factor(&mut self, factor: f64) {
        self.scale.set(factor);
        self.requests.scale(factor);
    }

    fn window_scale(&self) -> Option<f64> {
        #[cfg(target_os = "linux")]
        if let Some((_, scale)) = self.native_wayland_size() {
            return Some(scale);
        }
        let open = self.window.as_ref().filter(|w| w.0.is_open())?;
        (!cfg!(target_os = "macos")).then(|| open.0.size().scale_factor)
    }

    fn state_changed(&mut self) {
        // A preset replaced what a gesture in flight was editing: end it,
        // and stop the drag so it cannot keep writing the old value.
        let mut s = lock(&self.shared);
        s.ui.cancel();
        s.view.bridge.end_all();
        self.requests.redraw();
    }

    fn param_at(&self, x: i32, y: i32) -> Option<u32> {
        let s = lock(&self.shared);
        let zoom = s
            .view
            .zoom(Size::new(f64::from(self.size.0), f64::from(self.size.1)));
        let zoom = if zoom.is_finite() && zoom > 0.0 {
            zoom
        } else {
            1.0
        };
        let point = mui::prelude::Point::new(f64::from(x) / zoom, f64::from(y) / zoom);
        let scene = s.ui.scene()?;
        // Walk topmost z-order first so overlapping controls resolve to the
        // visible widget; ascend `parent` for composite sub-surfaces.
        for surface in scene.surfaces().rev() {
            if surface.disabled {
                continue;
            }
            if let Some(clip) = surface.clip
                && (point.x < clip.x0
                    || point.x > clip.x1
                    || point.y < clip.y0
                    || point.y > clip.y1)
            {
                continue;
            }
            let local = surface.local(point);
            if local.x < 0.0
                || local.y < 0.0
                || local.x > surface.frame.size.width
                || local.y > surface.frame.size.height
            {
                continue;
            }
            let mut key = Some(surface.key.as_str());
            while let Some(k) = key {
                if let Some(param) = s.view.bridge.param_for_widget(k) {
                    return Some(param);
                }
                key = scene.surface(k).and_then(|n| n.parent.as_deref());
            }
        }
        None
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "screenshot dimensions are bounded positive pixel counts"
    )]
    fn screenshot(&mut self, _params: Arc<dyn Params>) -> Option<(Vec<u8>, u32, u32)> {
        let scale = moose_core::screenshot::override_scale().unwrap_or(1.0);
        let (lw, lh) = self.size;
        if lw == 0 || lh == 0 {
            return None;
        }
        let pw = (f64::from(lw) * scale).round().max(1.0) as u32;
        let ph = (f64::from(lh) * scale).round().max(1.0) as u32;
        let window = Size::new(f64::from(lw), f64::from(lh));
        let mut s = lock(&self.shared);
        let zoom = s.view.zoom(window);
        let zoom = if zoom.is_finite() && zoom > 0.0 {
            zoom
        } else {
            1.0
        };
        let logical = Size::new(window.width / zoom, window.height / zoom);
        let s = &mut *s;
        let root = s.view.build(&mut s.ui, &Input::default());
        let frame =
            s.ui.frame(root, Some(logical), Input::default(), 0.0)
                .ok()?;
        let mut renderer = mui::vello::software::Renderer::new((pw, ph)).ok()?;
        let transform = mui::vello::kurbo::Affine::scale(scale * zoom);
        renderer.render(frame.scene, transform).ok()?;
        Some((renderer.rgba8(), pw, ph))
    }
}

impl<P: Params> Drop for MuiEditor<P> {
    fn drop(&mut self) {
        self.publish_win32_parent(None);
        // The host may have torn its side down already: no callbacks here.
        {
            let mut s = lock(&self.shared);
            s.view.bridge.detach();
            if let Some(f) = &mut s.view.closed {
                f();
            }
        }
        self.close_window();
    }
}

#[cfg(test)]
mod tests;
