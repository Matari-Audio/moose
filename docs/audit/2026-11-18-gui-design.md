# MOOSE editor integration: spec audit and design (2026-11-18)

Scope: the editor contract (`moose-core/src/editor.rs`), how CLAP `gui`, the
VST3 `IPlugView`, the AU v2 Cocoa view and the standalone window drive it, and
the MUI / baseview hosting underneath, on macOS, Windows, Linux X11 and Linux
Wayland. Companion to `2026-11-18-api-redesign.md` (B5, F1, F2).

Owners: the "gui" agent owns this document, the `Editor` trait and
`PluginContext`, `moose-gui*`, `moose-mui`, `moose-egui`, the standalone
window, and the GUI code of `moose-clap` and `moose-au`. The VST3 view object
belongs to "bindings" (rewrite onto the official SDK); its required behaviour
is written down in section 4 and in `DECISIONS.md`.

Spec sources:

- CLAP 1.2.x `include/clap/ext/gui.h` (cited as *gui.h*), `ext/timer-support.h`,
  `ext/posix-fd-support.h`, `ext/state.h`, `ext/context-menu.h`.
- VST3 SDK 3.8 `pluginterfaces/gui/iplugview.h`, `iplugviewcontentscalesupport.h`,
  `pluginterfaces/gui/iwaylandframe.h`, `pluginterfaces/base/funknown.h`,
  `pluginterfaces/vst/ivsteditcontroller.h` (`IComponentHandler2/3`), and the
  dev-portal pages "Linux: run loop" and "Wayland support (3.8)".
- Apple `AudioUnit/AUCocoaUIView.h` (`AUCocoaUIBase`), AUv3 `AUViewController`.

---

## 1. Matrix: what the spec requires vs what MOOSE does

Legend: OK, DEFECT (numbered in section 2), GAP (missing feature, deferred
with reason), N/A.

### 1.1 Embedding and parenting

| Format / OS | Spec | MOOSE | Verdict |
|---|---|---|---|
| CLAP, all | `create(api, is_floating)` then `set_parent(window)` returns `true` only if the plugin's window now exists under `window` (*gui.h*: "Returns true on success") | `gui_set_parent` returns `true` after `Editor::open`, which returns `()`. A failed baseview / wgpu window (no GPU, bad parent handle) still reports success, so the host shows an empty frame | DEFECT G1 |
| CLAP macOS | `CLAP_WINDOW_API_COCOA`, `clap_window.cocoa` is an `NSView*` | AppKit handle, child pinned to the parent's top (`anchor_child_to_top`) | OK |
| CLAP Windows | `CLAP_WINDOW_API_WIN32`, `HWND` | Win32 handle | OK |
| CLAP Linux X11 | `CLAP_WINDOW_API_X11`, `clap_window.x11` is an X11 `Window` id | X11 id via `specific.ptr`, embedded with XEmbed-style reparent on a private connection | OK |
| VST3, all | `attached(parent, type)` returns `kResultOk` only when the view was created; `kResultFalse` otherwise | Rust `gui_open` returns `void`, shim returns `kResultOk` unconditionally | DEFECT G1 (bindings side, contract in 4) |
| VST3 | `isPlatformTypeSupported`: `"NSView"`, `"HWND"`, `"X11EmbedWindowID"`, 3.8 `"WaylandSurfaceID"` | First three; Wayland not supported | GAP W1 |
| AU v2 | `uiViewForAudioUnit:withSize:` returns an autoreleased `NSView` the host owns; the plugin tears its UI down when that view leaves its window or is released | View container created and the editor opened in it, but **`gui_close` is never called**: nothing in `au_v2_view.m` observes the view leaving its window. The editor (render timers, wgpu surface) keeps running against an orphaned view until the next open or instance disposal | DEFECT G2 |
| AU v2 | If the editor cannot be created, return `nil` | View returned even when `Editor::open` failed | DEFECT G1 |
| AUv3 | `AUViewController` view in the appex | Generated Swift template (`cargo-moose/templates/au3`) | Not audited in depth (needs macOS + Xcode) |
| Standalone | Owns a top-level window | baseview top-level on X11/Win32/AppKit, editor opened as its child | OK on X11; no native Wayland top-level (GAP W3) |

### 1.2 Logical vs physical size, HiDPI

| Format / OS | Spec | MOOSE | Verdict |
|---|---|---|---|
| CLAP macOS | Cocoa API is logical; `set_scale` "should not be used if the windowing API relies upon logical pixels", return `false` | Refuses `set_scale` on macOS, reports logical size | OK |
| CLAP Win/Linux | Sizes are physical pixels; `set_scale` gives the absolute factor; the plugin may ignore it and query the OS, returning `false` | Accepts, stores it, converts logical↔physical with it; without it uses the editor's window scale (monitor DPI / `Xft.dpi`) | OK |
| CLAP Win/Linux | After the scale changes while the GUI is open, the plugin's physical size changes and the host frame must follow (host may re-query `get_size`, but nothing obliges it to) | `gui_set_scale` updates the editor but does not tell the host the new physical size; VST3 does (`resizeView` after `setContentScaleFactor`) | DEFECT G3 |
| CLAP floating | `set_scale` is meaningless for floating windows | Refused for floating | OK |
| VST3 macOS | `ViewRect` is in points; `IPlugViewContentScaleSupport` is not used on macOS | Logical sizes; scale ignored on macOS (shim still returns `kResultOk`) | OK, but bindings should return `kResultFalse` on macOS (4) |
| VST3 Win/Linux | `ViewRect` physical; `setContentScaleFactor` may arrive before or after `attached`, and again on monitor change | Stored on the instance, replayed into new editors and re-synced around `open`, `resizeView` sent on change | OK |
| Windows per-monitor DPI v2 | Child HWNDs of a PMv2 host receive `WM_DPICHANGED_AFTERPARENT` (not `WM_DPICHANGED`) when the top level crosses monitors | baseview handles `WM_DPICHANGED_AFTERPARENT`: re-reads DPI, keeps the logical size, resizes the child physically unless the host pinned a scale | OK |
| Windows mixed-DPI, host without a scale API | The host frame must be resized to the child's new physical size | Child resizes itself; the host frame is not told. Hosts that call `set_scale` / `setContentScaleFactor` on a monitor move (Cubase, Live, Bitwig, REAPER for VST3) are fine | GAP H1 (narrow; deferred, see 5) |
| Linux X11 | No standard per-window scale; `Xft.dpi` is the convention | `Xft.dpi` via baseview when no host scale | OK |
| AU | AppKit points; backing scale is the OS's | Logical, no pinning | OK |

### 1.3 Resize negotiation

| Format | Spec | MOOSE | Verdict |
|---|---|---|---|
| CLAP `can_resize` / `get_resize_hints` | [main-thread & !floating]; hints say per-axis resizability and aspect | `can_resize_horizontally/vertically` always `true` when resizable, even when `min_size == max_size` on that axis | DEFECT G4 |
| CLAP `adjust_size` | Clamp to the nearest acceptable size, return `true` | Fit to min/max/aspect, physical↔logical | OK |
| CLAP `set_size` | Host commits; returns `true` if the plugin resized to that size | Applies the fit; if the fit differs, asks the host for the fitted size | OK |
| CLAP plugin-initiated | `host_gui.request_resize` [thread-safe & !floating]; host answers with `set_size` (possibly synchronously) | `request_resize` callable from any thread; same-thread re-entry stashed in `pending_resize` | OK |
| VST3 `checkSizeConstraint` | Clamp the rect in place, `kResultTrue` | Fit inside the request; fixed editors snap to current size | OK |
| VST3 `onSize` | Host committed; must not call `resizeView` from inside | Clamp only, no re-request | OK |
| VST3 plugin-initiated | `IPlugFrame::resizeView` on the UI thread; host calls `onSize` back | Queued to the UI thread when called off it (shim) | OK (bindings must keep the queue, 4) |
| AU v2 | No host resize protocol | Fixed container at the editor's size | OK (documented limitation) |

### 1.4 Show / hide / destroy ordering

| Format | Spec | MOOSE | Verdict |
|---|---|---|---|
| CLAP embedded | create → set_scale → get_size → set_parent → show … hide → destroy; `destroy` may come without `hide` | `show`/`hide` are no-ops for embedded (the host hides the parent window); `destroy` closes and drops | OK |
| CLAP floating | create → suggest_title → show → hide → destroy; `host_gui.closed(was_destroyed)` when the user closes the window | `show` opens the toplevel, `hide` closes it (the model stays), compositor close marshalled to the main thread and reported with `was_destroyed = false` | OK |
| VST3 | `attached` / `removed` may repeat on one `IPlugView`; `setFrame` before `attached`, `setFrame(nullptr)` before release | `removed` closes; a re-`attached` re-opens | OK |
| AU v2 | View lifetime is the host's | Never closed (G2) | DEFECT G2 |

### 1.5 Event loop and timers on Linux

| Format | Spec | MOOSE | Verdict |
|---|---|---|---|
| CLAP | `timer-support` and `posix-fd-support` let a plugin run its GUI on the host's main loop; using them is optional. All `clap_host_*` calls keep their thread rules | The editor runs its own X11 connection on its own thread (`ThreadedWindow`). Everything it calls on the host is thread-safe by spec (`request_flush`, `request_resize`, `request_callback`) or marshalled through `request_callback` → `on_main_thread` (floating `closed`, `state.mark_dirty`) | OK by design (D-GUI-3) |
| VST3 | The SDK expects plugins to get UI-thread callbacks from the host's `IRunLoop` (queried from `IPlugFrame`): `registerTimer`, `registerEventHandler(fd)`. `IComponentHandler` and `IPlugFrame` must be called on the UI thread | Own editor thread as above; edits, restarts and resize requests are queued and drained on the UI thread through an eventfd registered with `IRunLoop` | OK (bindings must keep this, 4) |
| VST3 without `IRunLoop` | Some hosts do not provide it | Falls back to the param-poll / edit-gesture flush | OK |

Why not run the editor on the host loop: MUI renders with wgpu and paces frames
off its own presentation clock; X11 traffic is on a private connection, so
there is no shared Xlib state. Driving it from host timers would tie frame
pacing to host timer resolution (REAPER: 30 Hz) for no correctness gain. The
contract that matters is the thread each host call runs on, and that is
enforced.

### 1.6 Floating windows

| Format | Spec | MOOSE | Verdict |
|---|---|---|---|
| CLAP X11 floating | Allowed | Supported (`open_floating_x11`) | OK |
| CLAP Wayland | Only floating (`CLAP_WINDOW_API_WAYLAND`, `is_floating = true`); `set_transient` takes a `wl_surface` the plugin cannot use on its own connection | Floating toplevel through `mui-winit`; `set_transient` returns `false` | OK |
| CLAP preferred API on Linux | Plugin's choice | Embedded X11 when `DISPLAY` is set (XWayland included), floating Wayland only without X | OK |
| Win/macOS floating | Optional | Not offered | OK |

### 1.7 Wayland

| Item | Spec | MOOSE | Verdict |
|---|---|---|---|
| CLAP | Floating only | Done (1.6) | OK |
| VST3 3.8 embedded | `kPlatformTypeWaylandSurfaceID`: `attached` gets the parent `wl_surface*`. The plugin gets a host-proxied `wl_display*` from `IWaylandHost::openWaylandConnection()` (queried from the host context), creates a `wl_subsurface` of the parent surface obtained through `IWaylandFrame` (queried from `IPlugFrame`), and dispatches that display's events through `IRunLoop::registerEventHandler(wl_display_get_fd(display))` | Not supported; MUI has no embedded Wayland backend (winit cannot parent to a foreign surface) | GAP W1 |
| Standalone | A pure-Wayland session (no XWayland) | baseview X11 only | GAP W3 |
| CLAP nested compositor (draft) | `clap-wayland-experiment`: `wayland-surface-embedding-v1`, the plugin connects on the host's `open_connection()` fd and calls `clap_surface_embedding_v1.get_embedded_surface(surface, token)`; the host pumps it via `posix-fd-support` / `timer-support` | Behind `clap-draft`: `wayland_nested.rs` in moose-clap, `EmbeddedWaylandWindow::open_nested` in baseview. Tested against a fake compositor (baseview) and a fake host (moose-clap); popups' `position` is ignored (no editor opens popups) | OK, untested in a real host |

### 1.8 Keyboard focus and key forwarding

| Platform | Spec / convention | MOOSE (baseview) | Verdict |
|---|---|---|---|
| macOS | Unhandled `keyDown:` goes up the responder chain (`[super keyDown:]`) so host shortcuts work | Done (`view.rs`) | OK |
| Windows | Keys the plugin does not use must reach the host's accelerators | A `WH_GETMESSAGE` hook on the host GUI thread claims only keys the editor captures; ignored keys stay in the host's message stream | OK |
| X11 | A child on its own connection must re-send unhandled keys to the parent (`XSendEvent`) and give focus back | `forward_key_event` re-sends ignored keys; `focus_handoff` returns focus | OK |
| VST3 `onKeyDown`/`onKeyUp`/`onWheel` | Return `kResultTrue` only if handled | `kResultFalse` (keys arrive natively) | OK |
| CLAP | No key API; native events | Native | OK |

### 1.9 Editor-to-host context (F2)

| Action | CLAP | VST3 | AU | MOOSE today | Verdict |
|---|---|---|---|---|---|
| begin / set / end edit | queued `PARAM_GESTURE_*` / `PARAM_VALUE` + `request_flush` | `beginEdit` / `performEdit` / `endEdit` on the UI thread | AU parameter listener notify | Implemented per wrapper | OK |
| request resize | `host_gui.request_resize` | `IPlugFrame::resizeView` | container size | Implemented | OK |
| mark dirty (custom state changed) | `host_state.mark_dirty` [main-thread] | `IComponentHandler2::setDirty(true)` [UI thread] | none; hosts poll `ClassInfo` | **Missing**: an editor that changes custom state (`set_state`) cannot tell the host the project is modified | DEFECT G5 |
| context menu for a param | `context-menu` ext (`host_context_menu.populate/perform/popup`) | `IComponentHandler3::createContextMenu` | none | Missing | GAP C1 (deferred, needs the CLAP binding layer and the VST3 rewrite) |
| request restart | `host.request_restart` | `restartComponent` | — | Out of GUI scope (redesign D1) | N/A |

### 1.10 Threading of editor callbacks

| Callback | Thread (spec) | MOOSE |
|---|---|---|
| All `clap_plugin_gui` | main thread | main thread; cell guard against same-thread re-entry |
| All `IPlugView` | UI thread | UI thread |
| Editor → bridge | any thread the editor runs on | Each `PluginContext` declares `EditorNotificationDelivery` (`HostThread`, `QueuedEdits`, `QueuedAll`) so the editor knows whether it must marshal |
| `Editor::state_changed` | main thread, after state load | main thread |

---

## 2. Defects and fixes (implemented in this branch unless noted)

| # | Sev | Defect | Spec | Fix |
|---|---|---|---|---|
| G1 | S1 | `Editor::open` cannot fail, so CLAP `set_parent`, VST3 `attached` and AU `uiViewForAudioUnit` report success with no window | *gui.h* `set_parent` "Returns true on success"; `IPlugView::attached` result codes | New `Editor::is_open(&self) -> bool` (default `true`, so existing editors keep today's answer). MUI, egui and the built-in moose-gui editors report the truth. CLAP `set_parent` returns it. AU: `gui_open` returns `i32`, the view factory returns `nil` on failure. VST3: contract for bindings (`gui_open` returns whether the view opened). |
| G2 | S1 | AU v2 never closes the editor when its view leaves the window | `AUCocoaUIBase`: the view is the host's; the plugin's UI lives as long as that view is in a window | Container overrides `viewDidMoveToWindow`: closes when the window goes away, reopens when it comes back. The Rust side remembers the container and detaches it in `destroy`, so a view outliving its AudioUnit never calls into freed memory. |
| G3 | S2 | CLAP `set_scale` while open leaves the host frame at the old physical size | *gui.h*: sizes are physical on Win32/X11; the plugin's window changed size | After a changed scale with an open embedded editor, `request_resize` to the editor's logical size at the new scale (same as VST3's `resizeView`). |
| G4 | S3 | Resize hints claim both axes resizable when one is fixed | `clap_gui_resize_hints.can_resize_horizontally/vertically` | Axis resizable iff `min != max` on that axis. |
| G5 | S2 | No way for the editor to mark the project modified | `clap_host_state.mark_dirty`; `IComponentHandler2::setDirty` | `PluginContext::mark_dirty()`, and `PluginContext::set_state` marks dirty after it hands the bytes over. CLAP: flag + `request_callback`, `host_state.mark_dirty` from `on_main_thread`. VST3: contract for bindings. AU and standalone: no host API, no-op. |

## 3. Contracts (logged in DECISIONS.md as DECISION entries)

- **D-GUI-1 units.** `Editor` sizes are logical points everywhere. Wrappers
  convert at the format boundary: macOS (all formats) and AU are logical;
  CLAP/VST3 on Windows/Linux are physical, converted with the host's scale if
  it sent one, else the editor's `window_scale()`, else 1.0. Never both.
- **D-GUI-2 open result.** After `Editor::open`, wrappers read
  `Editor::is_open()`; `false` means the format's failure result (`false`,
  `kResultFalse`, `nil`). Default `true`.
- **D-GUI-3 threads.** Every `Editor` method is called on the host's GUI /
  main thread. The editor may run its own event-loop thread (Linux). Calls it
  makes into `PluginContext` may come from that thread; the wrapper either
  makes them thread-safe or declares `EditorNotificationDelivery::HostThread`
  and the editor marshals. Wrappers never call host GUI APIs off the host's
  UI thread.
- **D-GUI-4 dirty.** `PluginContext::mark_dirty()` is callable from any
  thread; wrappers deliver it on the host's main/UI thread.
  `PluginContext::set_state` calls it.
- **D-GUI-5 Wayland.** CLAP: floating only. VST3: embedded
  `"WaylandSurfaceID"` is unsupported until an editor reports support; the
  view must answer `kResultFalse` for it so hosts fall back to X11/XWayland.

## 4. What the VST3 `IPlugView` must do (for "bindings")

Also logged in `DECISIONS.md` to bindings.

1. `isPlatformTypeSupported`: `kResultTrue` for `"NSView"` (macOS), `"HWND"`
   (Windows), `"X11EmbedWindowID"` (Linux); `kResultFalse` for
   `"WaylandSurfaceID"` and everything else.
2. `attached(parent, type)`: reject a null parent or an unsupported type
   before calling Rust; call `cb_gui_open`; return `kResultOk` only when the
   editor reports open (`Editor::is_open()` after `open`; make the Rust
   callback return it). `removed()`: call `cb_gui_close`; idempotent, and a
   later `attached` on the same view must work.
3. `getSize`, `onSize`, `checkSizeConstraint`, `canResize`: forward to the
   existing Rust callbacks unchanged (they already do the physical↔logical
   conversion and the fit). `checkSizeConstraint` always returns
   `kResultTrue` after clamping; `onSize` never calls `resizeView`.
4. `setFrame`: keep (addRef) the frame; plugin-initiated resizes call
   `IPlugFrame::resizeView` on the UI thread only, queued when the request
   comes from another thread.
5. `IPlugViewContentScaleSupport::setContentScaleFactor`: forward to
   `cb_gui_set_content_scale` on Windows/Linux and return `kResultTrue`;
   return `kResultFalse` on macOS.
6. Linux `IRunLoop` (from `IPlugFrame`): register the eventfd that drains
   queued `beginEdit`/`performEdit`/`endEdit`, `restartComponent` and
   `resizeView` calls; unregister on `setFrame(nullptr)` / release. No GUI
   rendering timer is needed: the editor paces itself.
7. `onKeyDown`, `onKeyUp`, `onWheel`: `kResultFalse`. `onFocus`: `kResultOk`.
8. `IComponentHandler2::setDirty(true)` for the new mark-dirty request
   (D-GUI-4), on the UI thread, when the host provides `IComponentHandler2`.
9. Wayland later (W1): when an editor reports embedded-Wayland support, accept
   `"WaylandSurfaceID"`, obtain `IWaylandHost` from the host context and
   `IWaylandFrame` from the plug frame, pass `(wl_display*, wl_surface*)` to
   the editor, and register `wl_display_get_fd` with `IRunLoop`.

## 5. Deferred, with reasons

- **W1, VST3 embedded Wayland.** Needs an MUI backend that renders into a
  `wl_subsurface` on a host-proxied `wl_display`, takes input from that
  connection's `wl_seat` and is dispatched from the host's `IRunLoop`. winit
  cannot do this; it is a new backend on `wayland-client` + raw wgpu surface
  handles, larger than this pass. Hosts fall back to X11/XWayland meanwhile.
- **W3, standalone on pure Wayland.** The standalone's outer window is a
  baseview X11 window. A no-XWayland session could reuse the editor's
  floating-Wayland path; deferred because every desktop that ships a DAW
  ships XWayland.
- **H1, mixed-DPI move on Windows with a host that sends no scale.** The
  child keeps its logical size but the host frame is not resized. Fixing it
  needs the editor to report its new scale to the wrapper before the resize
  request; deferred until a host that needs it is identified.
- **C1, host context menus.** Belongs with the CLAP binding layer
  (`context-menu` ext) and the VST3 rewrite (`IComponentHandler3`).

---

## 6. Log

### Changed (branch `gui/editor-contract`)

- `moose-core/src/editor.rs`: `Editor::is_open` (default `true`, forwarded by
  `TaskBoundEditor`); `PluginContext::mark_dirty`, `with_mark_dirty`, type
  `MarkDirty`; `PluginContext::set_state` marks dirty. Test
  `set_state_marks_dirty_and_survives_adapters`.
- `moose-mui`, `moose-egui`, `moose-gui` (`editor.rs`, `gpu_editor.rs`):
  `is_open` reports whether the window exists.
- `moose-clap`: `set_parent` returns `is_open()` (G1); `set_scale` asks for a
  host resize when the scale changed with an open embedded editor (G3);
  per-axis resize hints (G4); `dirty_pending` + `with_mark_dirty` +
  `host_state.mark_dirty` in `on_main_thread` (G5).
- `moose-au` + `moose-shim-types/au_shim_types.h`: `gui_open` returns
  `int32_t`; `au_v2_view.m` returns `nil` on failure, closes the editor when
  the view leaves its window or is deallocated, reopens when it returns;
  `moose_au_view_detach` called from `cb_destroy` (G1, G2).
- `CHANGELOG.md` Unreleased entries.

### Skipped, and why

- VST3 view changes (G1 open result, `setDirty`, macOS `kResultFalse` for
  content scale): owned by "bindings", requested in `DECISIONS.md`.
- W1 / W3 / H1 / C1: see section 5.
- No new `Editor::set_visible` for CLAP embedded `show`/`hide`: hosts hide the
  parent window. Revisit if a host is found that leaves a hidden editor rendering.

### Not compiled or not tested

- `moose-au` for macOS: its build script compiles C/Objective-C and this Linux
  machine has no macOS SDK (`xcrun` missing), so neither the Rust nor the
  `.m` changes were compiled. Reviewed by hand only. Untested in any AU host.
- The CLAP and editor changes compiled for `x86_64-pc-windows-msvc` and
  `aarch64-apple-darwin` (`cargo check`), but were not run in a host on
  Windows or macOS.
- Linux: unit tests only; not exercised in a DAW in this pass.

### Commands

- `build-slot cargo test -q -p moose-core --lib editor`: 11 passed.
- `build-slot cargo test -q -p moose-clap --lib gui`: 5 passed.
- `build-slot cargo check -q -p moose-clap -p moose-au -p moose-mui -p moose-egui -p moose-gui --tests`: ok.
- `build-slot cargo check -q --target x86_64-pc-windows-msvc -p moose-clap -p moose-mui -p moose-egui -p moose-gui`: ok.
- `build-slot cargo check -q --target aarch64-apple-darwin -p moose-clap -p moose-mui -p moose-egui -p moose-gui`: ok.
- `... --target aarch64-apple-darwin -p moose-au`: fails in the build script (no macOS SDK), as expected.

### Wayland phase 1 (branch `gui/wayland`, after the research doc)

- baseview's native Wayland runtime (previously dock only) gains an xdg
  toplevel role in `platform/layer/toplevel.rs`:
  - server decorations, or the toolkit's fallback frame on subsurfaces when the compositor draws none
  - fractional scale through viewporter
  - min/max size hints
  - title changes
  - `defer_user_close` handling
- `ThreadedWindow` opens it with `WindowSettings::with_wayland(true)`.
- CLAP floating Wayland editors use it through `mui_baseview::wayland::open`, in
  place of mui-winit. Handler, presenters and the request queue are the X11
  path's. mui-baseview no longer depends on winit.
- W3: on a session with `WAYLAND_DISPLAY` and no `DISPLAY`, the standalone opens
  its editor as that native toplevel. QWERTY MIDI and the outer window's keyboard
  shortcuts are not available on that path.
- Two pacing bugs found on Hyprland and fixed:
  1. A frame callback never arrives before the surface's first buffer, so
     frames are not gated on it until the first callback.
  2. A commit that paints nothing earns no callback. Explicit redraw and resize
     requests wait at most 50 ms; continuous animation stays callback-paced.
- A size the compositor imposes (tiled, maximized) wins over a client resize.
- Deferred, per the research doc:
  - The host-loop fd API (`event_fd` / `on_fd`) is deferred to VST3
    `WaylandSurfaceID`, the case where the host's `wl_display` is shared.
  - The CLAP nested-compositor draft is deferred behind `clap-draft`.
- Tested: `native_wayland_multiple_instances_resize_close_and_reopen` on Hyprland
  (1.5× fractional scale, tiled), plus the baseview and mui-baseview unit tests.
  The volt standalone was run with `DISPLAY` unset: it opened as a native client
  (Hyprland reports `xwayland: false`) and exited when the compositor closed it.
  Not tested: GNOME/KDE (the fallback frame path), any DAW.
