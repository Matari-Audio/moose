//! Host-facing input policy for egui editors.
//!
//! Keyboard events always reach egui. This module controls only whether the
//! embedded child reports each event as captured to the host, so an editor can
//! keep its own shortcuts without stealing the DAW's remaining keys.

pub use keyboard_types::Key;

/// Which keyboard events the editor consumes instead of returning to the host.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum KeyCapture {
    /// Consume every keyboard event. This preserves the historical Moose
    /// behavior for editors that do not set an explicit policy.
    #[default]
    CaptureAll,
    /// Deliver events to egui, but return all of them to the host as ignored.
    IgnoreAll,
    /// Consume only the listed logical keys.
    CaptureKeys(Vec<Key>),
    /// Consume every logical key except the listed keys.
    IgnoreKeys(Vec<Key>),
}

/// Set the keyboard-capture policy for this editor context.
///
/// Call from the egui UI callback. The policy persists across frames and takes
/// effect on the next initial key-down. A captured key keeps that ownership
/// through repeats and its matching key-up, even if the policy changes while
/// held. It does not affect text/key delivery to egui; it controls only whether
/// the host also receives the key.
pub fn set_key_capture(context: &egui::Context, policy: KeyCapture) {
    context.data_mut(|data| data.insert_temp(key_capture_id(), policy));
}

/// Whether this backend can preserve physical key ownership through native
/// autorepeat and focus changes.
#[must_use]
pub const fn key_capture_available() -> bool {
    !cfg!(target_os = "ios")
}

/// Whether this backend receives native file drag/drop events.
///
/// baseview dispatches them on Windows, macOS and X11.
#[must_use]
pub const fn file_drop_available() -> bool {
    !cfg!(target_os = "ios")
}

#[cfg(not(target_os = "ios"))]
pub(crate) fn captures(context: &egui::Context, key: &Key) -> bool {
    match context.data(|data| data.get_temp::<KeyCapture>(key_capture_id())) {
        None | Some(KeyCapture::CaptureAll) => true,
        Some(KeyCapture::IgnoreAll) => false,
        Some(KeyCapture::CaptureKeys(keys)) => keys.contains(key),
        Some(KeyCapture::IgnoreKeys(keys)) => !keys.contains(key),
    }
}

/// Whether the native window should take keyboard input from the host
/// right now (baseview `set_keyboard_capture`, Windows only in effect).
///
/// True while egui wants the keyboard (a focused text field) and under the
/// default `CaptureAll` / `IgnoreKeys` policies; false for `IgnoreAll`, and
/// for `CaptureKeys` while nothing is focused.
// ponytail: `CaptureKeys` can't be per-key on Windows (the hook decides
// before the key is seen), so listed keys reach egui only while it has focus.
#[cfg(not(target_os = "ios"))]
pub(crate) fn wants_native_capture(context: &egui::Context) -> bool {
    if context.egui_wants_keyboard_input() {
        return true;
    }
    match context.data(|data| data.get_temp::<KeyCapture>(key_capture_id())) {
        None | Some(KeyCapture::CaptureAll | KeyCapture::IgnoreKeys(_)) => true,
        Some(KeyCapture::IgnoreAll | KeyCapture::CaptureKeys(_)) => false,
    }
}

fn key_capture_id() -> egui::Id {
    egui::Id::new(("moose-egui", "host-key-capture"))
}
