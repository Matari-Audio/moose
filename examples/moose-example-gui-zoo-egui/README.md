# egui GUI Zoo

The `Host platform` section is the copyable reference for selective DAW key
capture, Windows/macOS file drops, and the owned Linux dialog lifecycle. The
owned baseview revision normalizes X11 repeat pairs before they reach egui.

```rust
set_key_capture(
    ui.ctx(),
    KeyCapture::CaptureKeys(vec![Key::Character(" ".into()), Key::Escape]),
);

if file_drop_available() {
    for file in ui.input(|input| input.raw.dropped_files.clone()) {
        if let Some(path) = file.path {
            // Import `path` off the audio thread.
        }
    }
}
```

Keep one `DialogService` in an `EditorUi` implementation. Call `request()`
with the current egui context from the UI and `try_result()` each frame. The
editor owns close cleanup and immediately reaps an open Linux child before
`EditorUi::closed()` runs. The service truthfully reports unavailable on
Windows and macOS until Moose has a cancellable, correctly parented native
lifecycle.

## Manual host verification

Build the example, then load both its CLAP and VST3 bundles in Bitwig and
REAPER on Linux.

1. Hold Space, change the capture policy while it remains down, and release
   it. The DAW must not receive a repeat or unmatched release after Moose
   captured the initial press.
2. Press a key outside the capture list. egui must display it while the DAW
   still receives its shortcut.
3. Open and cancel the Zenity dialog, then open it again and select a path.
   Close the plugin while the dialog is open; the child must close and no
   `zenity` process may remain.

Record the exact host versions and results in the PR's `Manual testing`
section. On Windows and macOS, verify key capture and drag a file over the
editor, leave, re-enter, and drop in the Tier 1 hosts: hover feedback must clear
on leave and the dropped path must appear once. The dialog button must remain
disabled there. On Linux, file drop must report unavailable rather than claim a
path the X11 backend cannot deliver.
