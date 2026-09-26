//! `BaseviewTranslator` - the windowing-toolkit-specific half of
//! `moose-gui`'s interaction surface. The platform-agnostic data
//! types (`InputEvent`, `MouseButton`, `Modifiers`, `WidgetRegion`,
//! `InteractionState`, `DragState`, `DropdownState`, `dispatch`, …)
//! live in [`moose_gui_types::interaction`] and are re-exported here
//! so existing `moose_gui::interaction::*` paths keep working.

pub use moose_gui_types::interaction::*;

// Baseview-event translator is macOS / Windows / Linux only. iOS
// delivers events via UIKit touch handlers in `editor_ios`.
#[cfg(not(target_os = "ios"))]
const DOUBLE_CLICK_MS: u128 = 300;
#[cfg(not(target_os = "ios"))]
const DOUBLE_CLICK_SLOP: f32 = 4.0;
#[cfg(not(target_os = "ios"))]
const WHEEL_LINE_PX: f32 = 20.0;

/// Stateful translator from baseview events to moose-gui's
/// platform-agnostic [`InputEvent`] stream.
#[cfg(not(target_os = "ios"))]
///
/// Exists because baseview does not carry a position on `ButtonPressed` /
/// `ButtonReleased` nor synthesize double-clicks.
///
/// baseview reports cursor positions in **physical** pixels. The
/// translator divides them by the scale the editor renders at, so emitted
/// `InputEvent`s carry **logical** coordinates that always line up with
/// what was drawn, whatever scale baseview itself believes in.
// All fields share a `last_` prefix because the struct's whole purpose
// is to remember the previous cursor / click - the prefix is meaningful,
// not redundant.
#[cfg(not(target_os = "ios"))]
#[allow(clippy::struct_field_names)]
#[derive(Default)]
pub struct BaseviewTranslator {
    last_cursor: (f32, f32),
    last_click_time: Option<std::time::Instant>,
    last_click_pos: (f32, f32),
}

#[cfg(not(target_os = "ios"))]
impl BaseviewTranslator {
    /// The last cursor position we saw from a `CursorMoved`, in logical
    /// points. Useful when a caller needs to query cursor state outside
    /// the event stream (e.g. for its own overlays).
    #[must_use]
    pub fn last_cursor(&self) -> (f32, f32) {
        self.last_cursor
    }

    /// Convert a baseview event into an [`InputEvent`]. Returns `None`
    /// for events moose-gui doesn't consume (keyboard, non-L/R/M mouse
    /// buttons, window lifecycle).
    ///
    /// `scale` is the editor's render scale (physical pixels per point).
    pub fn translate(&mut self, event: &baseview::Event, scale: f64) -> Option<InputEvent> {
        let baseview::Event::Mouse(m) = event else {
            return None;
        };
        match m {
            baseview::MouseEvent::CursorMoved { position, .. } => {
                let logical = position.to_logical::<f64>(sanitize_scale(scale));
                // The hit-test math is f32. Window dimensions never reach
                // 2^23, so the narrowing is invisible.
                #[allow(clippy::cast_possible_truncation)]
                let x = logical.x as f32;
                #[allow(clippy::cast_possible_truncation)]
                let y = logical.y as f32;
                self.last_cursor = (x, y);
                Some(InputEvent::MouseMove {
                    pointer_id: moose_gui_types::interaction::SINGLE_POINTER,
                    x,
                    y,
                })
            }
            baseview::MouseEvent::ButtonPressed { button, .. } => {
                let mb = map_button(*button)?;
                let (x, y) = self.last_cursor;
                if mb == MouseButton::Left {
                    let now = std::time::Instant::now();
                    let is_double = self.last_click_time.is_some_and(|t| {
                        now.duration_since(t).as_millis() < DOUBLE_CLICK_MS
                            && (x - self.last_click_pos.0).abs() < DOUBLE_CLICK_SLOP
                            && (y - self.last_click_pos.1).abs() < DOUBLE_CLICK_SLOP
                    });
                    self.last_click_time = Some(now);
                    self.last_click_pos = (x, y);
                    if is_double {
                        self.last_click_time = None;
                        return Some(InputEvent::MouseDoubleClick { x, y });
                    }
                }
                Some(InputEvent::MouseDown {
                    pointer_id: moose_gui_types::interaction::SINGLE_POINTER,
                    x,
                    y,
                    button: mb,
                })
            }
            baseview::MouseEvent::ButtonReleased { button, .. } => {
                let mb = map_button(*button)?;
                let (x, y) = self.last_cursor;
                Some(InputEvent::MouseUp {
                    pointer_id: moose_gui_types::interaction::SINGLE_POINTER,
                    x,
                    y,
                    button: mb,
                })
            }
            baseview::MouseEvent::WheelScrolled { delta, .. } => {
                let dy = match delta {
                    baseview::ScrollDelta::Lines { y, .. } => y * WHEEL_LINE_PX,
                    baseview::ScrollDelta::Pixels { y, .. } => *y,
                };
                let (x, y) = self.last_cursor;
                Some(InputEvent::Scroll { x, y, dy })
            }
            baseview::MouseEvent::CursorLeft => Some(InputEvent::MouseLeave),
            _ => None,
        }
    }
}

#[cfg(not(target_os = "ios"))]
fn sanitize_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 { scale } else { 1.0 }
}

#[cfg(not(target_os = "ios"))]
fn map_button(b: baseview::MouseButton) -> Option<MouseButton> {
    match b {
        baseview::MouseButton::Left => Some(MouseButton::Left),
        baseview::MouseButton::Right => Some(MouseButton::Right),
        baseview::MouseButton::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}

#[cfg(all(test, not(target_os = "ios")))]
mod translator_tests {
    use super::*;

    #[test]
    fn physical_positions_become_logical() {
        let mut t = BaseviewTranslator::default();
        let ev = baseview::Event::Mouse(baseview::MouseEvent::CursorMoved {
            position: baseview::dpi::PhysicalPosition::new(300.0, 150.0),
            modifiers: keyboard_types::Modifiers::empty(),
        });
        let Some(InputEvent::MouseMove { x, y, .. }) = t.translate(&ev, 1.5) else {
            panic!("expected MouseMove");
        };
        assert!((x - 200.0).abs() < 1e-4 && (y - 100.0).abs() < 1e-4);
        // A bogus scale falls back to 1:1 instead of dividing by zero.
        let Some(InputEvent::MouseMove { x, .. }) = t.translate(&ev, 0.0) else {
            panic!("expected MouseMove");
        };
        assert!((x - 300.0).abs() < 1e-4);
    }
}
