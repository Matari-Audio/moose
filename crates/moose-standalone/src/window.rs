//! Top-level standalone window adapter on top of `moose-baseview`.
//!
//! baseview 0.3 calls its [`baseview::WindowHandler`] through `&self`, and a
//! handler call can re-enter the handler (a `WindowContext::resize` inside
//! `on_frame` delivers `resized` synchronously on Windows). The standalone
//! outer window keeps a `&mut self` handler, so [`EditorWindowHandler`] is the
//! `&mut` shape and [`open_window`] wraps it in a `RefCell` adapter. A call
//! that arrives while the handler is already borrowed is not dropped: a resize
//! is kept (latest wins) and events are queued, and both are delivered as soon
//! as the outer call returns. The adapter also catches panics.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use baseview::{Event, EventStatus, HandlerError, WindowContext, WindowSize};

/// A `&mut self` window handler, driven through [`open_window`].
pub(crate) trait EditorWindowHandler: 'static {
    /// Draw a frame. Called from baseview's frame timer.
    fn on_frame(&mut self, window: &WindowContext);

    /// Handle an input or window event.
    fn on_event(&mut self, window: &WindowContext, event: Event) -> EventStatus;

    /// The window was resized, or its scale factor changed its physical size.
    fn resized(&mut self, window: &WindowContext, size: WindowSize) {
        let _ = (window, size);
    }
}

struct Adapter<H> {
    ctx: WindowContext,
    handler: RefCell<H>,
    pending_resize: Cell<Option<WindowSize>>,
    pending_events: RefCell<VecDeque<Event>>,
}

impl<H: EditorWindowHandler> Adapter<H> {
    /// Deliver what arrived while the handler was busy.
    fn drain(&self, handler: &mut H) {
        if let Some(size) = self.pending_resize.take() {
            firewall("resized", || handler.resized(&self.ctx, size));
        }
        loop {
            let Some(event) = self.pending_events.borrow_mut().pop_front() else {
                break;
            };
            firewall("on_event", || handler.on_event(&self.ctx, event));
        }
    }
}

impl<H: EditorWindowHandler> baseview::WindowHandler for Adapter<H> {
    fn on_frame(&self) -> Result<(), HandlerError> {
        if let Ok(mut handler) = self.handler.try_borrow_mut() {
            self.drain(&mut handler);
            firewall("on_frame", || handler.on_frame(&self.ctx));
            self.drain(&mut handler);
        }
        Ok(())
    }

    fn resized(&self, size: WindowSize) -> Result<(), HandlerError> {
        match self.handler.try_borrow_mut() {
            Ok(mut handler) => {
                firewall("resized", || handler.resized(&self.ctx, size));
                self.drain(&mut handler);
            }
            Err(_) => self.pending_resize.set(Some(size)),
        }
        Ok(())
    }

    fn on_event(&self, event: Event) -> EventStatus {
        let Ok(mut handler) = self.handler.try_borrow_mut() else {
            self.pending_events.borrow_mut().push_back(event);
            return EventStatus::Ignored;
        };
        let status = firewall("on_event", || handler.on_event(&self.ctx, event))
            .unwrap_or(EventStatus::Ignored);
        self.drain(&mut handler);
        status
    }
}

/// Run `f`, logging and swallowing a panic instead of unwinding through native callbacks.
fn firewall<R>(what: &str, f: impl FnOnce() -> R) -> Option<R> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(e) => {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            eprintln!("moose-standalone window {what} panic swallowed: {msg}");
            None
        }
    }
}

/// An open standalone window. Dropping (or [`Self::run_until_closed`]) destroys it.
pub(crate) struct StandaloneWindow(baseview::Window);

impl StandaloneWindow {
    /// Run the window's event loop until it closes.
    ///
    /// # Errors
    ///
    /// Returns the platform error if the event loop fails.
    pub(crate) fn run_until_closed(self) -> Result<(), baseview::Error> {
        self.0.run_until_closed()
    }
}

/// Open a top-level standalone window from explicit `settings`.
pub(crate) fn open_window<H: EditorWindowHandler>(
    settings: baseview::WindowSettings,
    build: impl FnOnce(&WindowContext) -> H + Send + 'static,
) -> Option<StandaloneWindow> {
    let window = baseview::Window::create(settings, move |ctx: WindowContext| {
        let handler = build(&ctx);
        Ok(Adapter {
            ctx,
            handler: RefCell::new(handler),
            pending_resize: Cell::new(None),
            pending_events: RefCell::new(VecDeque::new()),
        })
    });
    let window = match window {
        Ok(window) => window,
        Err(e) => {
            eprintln!("moose-standalone: failed to create window: {e}");
            return None;
        }
    };
    if let Err(e) = window.show() {
        eprintln!("moose-standalone: failed to show window: {e}");
    }
    Some(StandaloneWindow(window))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firewall_swallows_panics() {
        assert_eq!(firewall("t", || 3), Some(3));
        assert_eq!(firewall::<()>("t", || panic!("boom")), None);
    }
}
