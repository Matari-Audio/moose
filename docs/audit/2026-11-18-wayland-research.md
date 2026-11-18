# Wayland plugin GUIs: state of the art and the design for MOOSE/MUI (2026-11-18)

Research for gap W1/W3 in `2026-11-18-gui-design.md`. Sources were read in full:
the CLAP #474 thread (all 242 comments), the `clap-wayland-experiment` headers
and protocol XML, VST3 `iwaylandframe.h`, VSTGUI's Wayland backend, Bacon
Paul's proof-of-concept host and plugins, and the REAPER/WDL Wayland port. The
source list is at the end.

## 1. What exists today

### Three ways to put a plugin GUI on Wayland

| | Floating toplevel | VST3 3.8 `WaylandSurfaceID` | Nested compositor + embedding protocol |
|---|---|---|---|
| Spec | CLAP `gui.h` today (`CLAP_WINDOW_API_WAYLAND`, floating only). Draft extensions add transient parenting and focus | VST3 SDK 3.8.0 (Oct 2025), written by PreSonus, still marked preview/beta | CLAP draft `clap-wayland-experiment` (Oct 2026): `CLAP_WINDOW_API_WAYLAND_SURFACE_EMBEDDING_V1`, protocol `clap_surface_embedding_v1`, to be renamed and proposed upstream as `xx_surface_embedding_v1` |
| Plugin's connection | Its own, to the system compositor | `wl_display*` from `IWaylandHost::openWaylandConnection()` (a connection to the host's compositor, opened inside libwayland-client) | A socket fd from `clap_host_gui_wayland_nested_compositor.open_connection()`; the plugin calls `wl_display_connect_to_fd` |
| Parent | `zxdg_imported_v2.set_parent_of` with the host's xdg-foreign handle (draft ext `clap.gui-xdg-foreign-v2/1`) | `attached(wl_surface* parent, "WaylandSurfaceID")`; plugin makes a `wl_subsurface` of it | A string token in `set_parent(token)`; plugin calls `get_embedded_surface(surface, token)` |
| Popups / dialogs | Normal xdg-shell | `IWaylandFrame::getParentSurface()` / `getParentToplevel()` | `clap_embedded_surface_v1.get_parent_surface()` / `get_parent_toplevel()` plus a `position` event |
| What crosses the format boundary | One string | libwayland-client objects (`wl_display*`, `wl_surface*`, `xdg_*`) | One fd and one string |
| ABI | Neutral | Tied to `libwayland-client.so.0`. abique (CLAP lead) rejected this design, and the PreSonus author agreed in the thread that a socket would be better | Neutral; any Wayland implementation works (pure-Rust `wayland-client` included) |
| Shipping hosts | Any host that offers CLAP floating Wayland | Studio One Pro 7.2+ on Linux; Steinberg EditorHost | Bitwig has implemented every scenario internally (not released as of the thread; its compositor isn't public). Bacon Paul's micro-host is a proof of concept. |
| Embedded? | No | Yes | Yes |

### The REAPER port (Phoronix, 2026-11-17)

Jason Donenfeld's `WDL-wayland` (pending merge into WDL/REAPER) is a native
Wayland backend for SWELL (libwayland-client + libdecor, 5.2k lines). It
**does not** implement any plugin Wayland API (no `IWaylandHost`, no CLAP
Wayland). Plugin editors stay X11: REAPER runs a **private rootless Xwayland**,
redirects editor windows with XComposite, repaints on XDamage, and imports the
pixels via DRI3 (fallback memfd MIT-SHM). So, for MOOSE on REAPER-Wayland, our
existing **embedded X11** path is the one that runs, and it must keep working
under a nested Xwayland (no assumptions about the root window, WM or
`_NET_*` hints; XEmbed focus and key forwarding as today).

The port is still a useful reference for the client side. It is event-driven:
a GLib source on `wl_display_get_fd`, `prepare_read`/`dispatch_pending`,
`wl_surface.frame` pacing, `preferred_buffer_scale`, `wp_viewporter`, and
desync subsurfaces. It was tested on KWin, Mutter, Weston, Hyprland, Sway,
dwl, labwc, niri, COSMIC and others.

### Toolkits and frameworks

- **VSTGUI** (Steinberg reference, VST3 3.8): one refcounted process-wide
  connection, `IRunLoop::registerEventHandler(wl_display_get_fd)`, on readable
  `prepare_read → read_events → dispatch_pending → flush`, `wl_subsurface`
  with `set_desync`, Cairo software rendering.
- **Bacon Paul's demos** (CLAP nested compositor): raw wl_shm, JUCE, SDL3,
  visage/bgfx and pugl all work with small changes. The recurring constraint
  is that **plugins share one connection across instances** (SDL and bgfx
  require it; the protocol supports it because tokens are per surface). The
  raw demo is fully event-driven: `posix-fd-support` for the socket, and a
  redraw on every `wl_surface.frame` callback.
- **GPU in a nested compositor:** SDL3 fell back to software until the host
  compositor forwarded `linux-dmabuf`. Hosts may not offer it, so a plugin must
  render correctly on `wl_shm` too.
- **JUCE:** nothing public for native Wayland editors. **RustAudio baseview:**
  issue #135 is open; prokopyl plans Wayland support and maintains `waybed`, a
  compositor proxy for hosts. MOOSE's vendored baseview would be early.

## 2. The gold standard: event-driven, atomic

"Event-driven" here means the editor costs nothing while nothing changes:
no timers, no polling and no thread of its own on the Wayland connection.
"Atomic" means each visible frame is one complete, consistent state, both on
screen (one `wl_surface.commit` per frame) and in memory (lock-free shared
state between audio and UI). What follows is what the reference
implementations converge on, plus what the protocols added since 2023.

### 2.1 One connection per process, dispatched by the host's loop

- One connection per (process, compositor), refcounted, shared by every
  editor instance (VSTGUI `useCount`; SDL/bgfx requirement). Each instance owns
  only its surfaces.
- Register `wl_display_get_fd(display)` with the host loop: VST3
  `IRunLoop::registerEventHandler` (the `IWaylandHost` calls are
  `[UI-thread]`, and the display is the host's, so this is required, not
  optional); CLAP `posix-fd-support.register_fd(READ | ERROR)`.
- On readable: `while prepare_read() != 0 { dispatch_pending() }`,
  `read_events()`, `dispatch_pending()`, then `flush()`. After queuing
  requests, `flush()`; on `EAGAIN`, also register WRITE interest and flush when
  writable. Never `wl_display_dispatch()` (it blocks).
- **No editor thread on the connection.** All Wayland object state lives on
  the host's UI thread, which is what both formats require of their host
  calls anyway. GPU submission may still happen on a render thread (wgpu's
  queue is thread-safe; Vulkan WSI uses its own event queue).

### 2.2 Frames are paced by the compositor

- Draw only on `wl_surface.frame` callbacks, and request a callback only when
  something is dirty. An idle, unchanged editor gets no wakeups at all. A
  hidden or occluded surface gets no frame callbacks, so it stops drawing on
  its own; this replaces CLAP `show`/`hide` heuristics.
- Present with `wp_fifo_v1` + `wp_commit_timing_v1` where offered (staging,
  2025) so at most one frame is queued and latency stays bounded; otherwise
  rely on frame callbacks alone. Use `wp_presentation` feedback to measure
  rather than guess.
- GPU buffers: `linux-dmabuf` with `linux-drm-syncobj-v1` explicit sync when
  available. Always keep a `wl_shm` path (memfd, double-buffered, accumulate
  damage the compositor held, as WDL does) for nested compositors without
  dmabuf forwarding.

### 2.3 Every frame is one atomic commit

- Attach, damage (`damage_buffer`), `wp_viewport` source/destination, buffer
  scale and input region all go into one `commit`.
- HiDPI is the compositor's: `wp_fractional_scale_v1.preferred_scale` with
  `wp_viewporter` (render at `ceil(logical × scale/120)`, set the destination
  to the logical size); fall back to `wl_surface.preferred_buffer_scale`
  (wl_compositor v6). Ignore CLAP `set_scale` (the draft says the embedding API
  uses logical sizes) and VST3 `setContentScaleFactor` on Wayland.
- **Resize without tearing against the host:** a `wl_subsurface` is
  desynchronized in steady state (both VSTGUI and WDL do this, so animation
  doesn't wait on the host's commits). During a host-driven resize (VST3
  `onSize`), switch to `set_sync`, commit the buffer at the new size, and let
  the host's parent commit apply both at once; back to `set_desync` on the
  next frame. With `clap_embedded_surface_v1` the compositor does this itself
  (the protocol says it may synchronize commits while the area resizes).

### 2.4 Shared state between audio and UI is lock-free and pull-based

- The audio thread never makes a syscall to wake the UI. It publishes values
  through atomics (MOOSE already does: param atomics, meter store, the
  per-instance edit epoch, `TransportSlot`).
- The UI pulls at frame time: compare the epoch, rebuild if it changed. While
  something animates (meters with audio running), frame callbacks keep
  coming. When fully idle, a **host timer armed only while the editor is
  visible and idle** (VST3 `IRunLoop::registerTimer`, CLAP `timer-support`,
  about 30 Hz) checks the epoch and requests a frame callback if it moved.
  That timer is the only periodic wakeup, and it touches one atomic.
- Editor → host goes through the queues MOOSE already has (CLAP gesture
  queue + `request_flush`; VST3 queued edits drained on the UI thread).

### 2.5 Input and keys

- Input arrives on the plugin's own `wl_seat` objects. The host never sees
  keys typed while the plugin has focus. Forward unconsumed keys with the
  draft `clap.gui-xkb-key-forwarding/1` (press/repeat/release pairs, XKB
  keycode + keysym + modifiers). VST3 has no equivalent yet.
- Floating windows: `xdg-activation-v1` tokens from the host for focus; the
  CLAP draft `clap.gui-xdg-activation-v1/1` carries them.

## 3. Design for MOOSE / MUI

### 3.1 One backend, three front doors

Each of the three ways reduces to "a `wl_surface` we own, on some
connection, given some role". So MUI gets one Wayland backend
(`mui-wayland`, replacing the winit floating adapter) parameterised by:

```rust
enum Connect {
    System,                       // floating: WAYLAND_DISPLAY / WAYLAND_SOCKET
    Socket(OwnedFd),              // CLAP nested compositor: open_connection()
    Foreign(NonNull<c_void>),     // VST3 3.8: IWaylandHost::openWaylandConnection()
}
enum Role {
    Toplevel { title: String, transient: Option<String>, activation: Option<String> },
    Subsurface { parent: NonNull<c_void> },  // VST3 attached(wl_surface*)
    Embedded { token: String },               // clap_surface_embedding_v1
}
```

- Crates: `wayland-client` 0.31 with the `system` backend (`Backend::
  from_foreign_display` for VST3; `Connection::from_socket` for the fd case;
  libwayland is loaded anyway by Vulkan/EGL WSI), `wayland-protocols` 0.32
  (xdg-shell, viewporter, fractional-scale, fifo, commit-timing,
  linux-drm-syncobj, xdg-activation, xdg-foreign), the
  `clap_surface_embedding_v1` XML through `wayland-scanner`, wgpu surfaces
  from raw `wl_display`/`wl_surface` handles, and MUI's existing CPU raster on
  `wl_shm`. No winit: it cannot take a foreign parent, a foreign display or
  an external event loop.
- The backend exposes `fd()`, `dispatch()` and `wants_write()` and never
  blocks. The wrapper drives them from the host loop (2.1).

### 3.2 Editor contract additions (moose-core)

```rust
enum RawWindowHandle { …, WaylandSubsurface { display: *mut c_void, parent: *mut c_void },
                          WaylandEmbedded { fd: RawFd, token: String } }
trait Editor {
    fn supports_api(&self, api: WindowApi) -> bool { … }        // replaces supports_floating_*
    fn event_fd(&self) -> Option<RawFd> { None }               // for the host loop
    fn on_fd(&mut self, readable: bool, writable: bool) {}
    fn on_timer(&mut self) {}                                    // idle epoch check
}
```

`RawWindowHandle` gains variants, which is breaking for exhaustive matches in
plugins. It should land as one change with the plugin fixes, or with
`#[non_exhaustive]` added first.

### 3.3 Per format

- **CLAP:** keep embedded X11 as the preferred API whenever `DISPLAY` is set
  (REAPER-Wayland and Bitwig embed X11 today). Floating Wayland moves
  onto the new backend and gains the xdg-foreign/xdg-activation draft
  extensions. Nested-compositor embedding goes behind `clap-draft` (names will
  change to `xx_surface_embedding_v1`). Implement host `posix-fd-support` and
  `timer-support` use for all Wayland modes.
- **VST3 (bindings):** accept `"WaylandSurfaceID"` only when the editor
  supports it and the host provides `IWaylandHost` (via
  `IPluginFactory3::setHostContext`/`IHostApplication::createInstance`).
  Register the display fd with `IRunLoop`, and close the connection when the
  last view goes. `IWaylandFrame` is used for popups. Studio One 7.2+ is the
  test host.
- **Standalone (W3):** the same backend with `Connect::System` +
  `Role::Toplevel` gives a native Wayland window without XWayland.

### 3.4 Phases

1. `mui-wayland` floating toplevel on the host loop (replaces winit floating
   in CLAP; standalone W3). Frame callbacks, fractional scale, shm + wgpu.
2. VST3 `WaylandSurfaceID` (needs the bindings rewrite): subsurface role,
   sync-on-resize, `IWaylandFrame` popups. Test in Studio One 7.2+.
3. CLAP nested compositor behind `clap-draft`, with key forwarding. Test with
   Bacon Paul's host, then Bitwig when it ships.
4. Retire the per-editor X11 thread in favour of the same host-loop model
   (xcb fd registered with `IRunLoop` / `posix-fd-support`), so both window
   systems share one event model.

### 3.5 Risks

- Both embedding specs are drafts: VST3's is "preview" and ABI-coupled to
  libwayland.so.0; CLAP's protocol name and extension ids will change.
  Feature-gate both.
- A nested compositor may forward few protocols (M × N × O × P, in abique's
  words): fractional-scale, dmabuf and cursor-shape may be missing. Every one
  needs a fallback (integer scale, shm, client-side cursor).
- One shared connection means one plugin instance's protocol error kills
  every instance's GUI. Validate everything before sending; never send a
  request on an object the compositor may have destroyed (`closed` events).

## Sources

- Phoronix, "REAPER Digital Audio Workstation Sees Native Wayland Port": https://www.phoronix.com/news/REAPER-DAW-Wayland
- WDL-wayland (Donenfeld): https://git.zx2c4.com/WDL-wayland (commits `cf2ada1d` X11 bridge, `88444081` native backend); forum thread https://forum.cockos.com/showthread.php?t=305832
- CLAP issue #474, "GUI on Wayland": https://github.com/free-audio/clap/issues/474
- clap-wayland-experiment: https://github.com/free-audio/clap-wayland-experiment
- Bacon Paul's host and demos: https://github.com/baconpaul/wayland_clap_protocol_host
- VST3 3.8 Wayland: https://steinbergmedia.github.io/vst3_dev_portal/pages//Technical+Documentation/Change+History/3.8.0/IWaylandHost.html and `pluginterfaces/gui/iwaylandframe.h`
- VSTGUI Wayland backend: `vstgui/lib/platform/linux/wayland*.cpp` in https://github.com/steinbergmedia/vstgui
- PreSonus reusable host delegate: https://github.com/cclsoftware/wayland-server-delegate
- waybed (prokopyl): https://github.com/prokopyl/waybed
- RustAudio baseview Wayland issue: https://github.com/RustAudio/baseview/issues/135
- wayland-protocols staging (fifo, commit-timing, fractional-scale, linux-drm-syncobj) as shipped in `wayland-protocols` 0.32.13
