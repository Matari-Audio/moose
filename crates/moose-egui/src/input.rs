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
/// baseview currently dispatches them on Windows and macOS only.
#[must_use]
pub const fn file_drop_available() -> bool {
    cfg!(any(target_os = "windows", target_os = "macos"))
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

fn key_capture_id() -> egui::Id {
    egui::Id::new(("moose-egui", "host-key-capture"))
}
