# MOOSE API redesign: from the truce port to official, typed foundations

Companion to `2026-11-18-audit.md`. That file lists bugs; this one lists what
is structurally wrong or fragile in the inherited truce design and what the
correct design is. Steps 1 to 6 of `2026-11-18-fix-plan.md` are patches inside
the current design. This document is the next pass.

## What changed in the outside world

- **VST3 SDK is MIT since 3.8.0 (2025-10-20).** 3.8.1 (2026-08-11) is current.
  The reason truce hand-wrote its own COM shim (GPLv3 or proprietary SDK
  licence) is gone. The official `pluginterfaces` headers, or bindings
  generated from them, can now ship in an MIT/Apache crate.
- **Rust bindings exist for both formats.**
  - `vst3` 0.3.0 (coupler-rs/vst3-rs, MIT/Apache): generated from the SDK
    headers by `com-scrape`, with COM helpers for implementing interfaces.
    Low level, pre-1.0.
  - `clap-sys` 0.5.0 (January 2025): what MOOSE uses. Plain C bindings.
  - `clack` 0.2.0 (September 2026, MIT/Apache): safe wrappers over `clap-sys`
    that encode CLAP's thread rules in types (`Shared`, `MainThread`,
    `AudioProcessor` created by `activate` and consumed by `deactivate`).
    Covers all stable extensions. Breaking changes still possible.
- **CLAP is at 1.2.10.** 1.2.0 stabilized ambisonic, audio-ports-activation,
  configurable-audio-ports, context-menu, param-indication, preset-load,
  remote-controls, state-context, surround, track-info and the
  preset-discovery factory. Drafts added since include undo, scratch-memory,
  gain-adjustment-metering, mini-curve-display, webview, param-origin,
  param-hovered, flush-events, background-activation, background-state-context,
  background-progress, project-location, octave-number.
- **VST3 3.8 adds** `IMidiLearn2` / `IMidiMapping2` (MIDI 2.0, replacing
  `IMidiLearn` / `IMidiMapping`), Wayland (`IWaylandHost`, `IWaylandFrame`,
  `kPlatformTypeWaylandSurfaceID`), and in 3.8.1 `ITransportControl` and
  MIDI 2.0 note-expression IDs.

## What MOOSE implements today

CLAP (via `clap-sys` 0.5): params, state, preset-load, remote-controls,
audio-ports, audio-ports-config, surround, note-ports, gui, latency, tail,
render, preset-discovery factory, and a hook for custom extensions. It uses
only the host's `params`, `latency`, `tail` and `state` extensions plus
`request_callback`. Not implemented: state-context, param-indication,
context-menu, track-info, note-name, voice-info, audio-ports-activation,
configurable-audio-ports, ambisonic, thread-pool, timer-support, posix-fd,
and the host's thread-check and log. No draft extensions.

VST3 (hand-written C++ COM in `moose-vst3/shim/vst3_shim.cpp`, no SDK):
`IComponent`, `IAudioProcessor`, `IEditController`, `IEditControllerHostEditing`,
`IMidiMapping`, `IProcessContextRequirements`, `IUnitInfo` (only with more than
one unit), `INoteExpressionController`, `IPlugView` with content-scale support,
`IPluginFactory` 1/2/3, host `IRunLoop` / timers. `restartComponent` is only
ever sent with `kLatencyChanged` and `kParamTitlesChanged`. Not implemented:
`IComponentHandler2` (`setDirty`, `startGroupEdit`), `IComponentHandler3`
(context menus), `IMidiLearn` / `IMidiLearn2` / `IMidiMapping2`,
`IParameterFunctionName`, `IParameterFinder`, `IRemapParamID`, `IInfoListener`
(track name and colour), `IPluginCompatibility`, `moduleinfo.json`,
`IProgramListData`, `IKeyswitchController`, `IAudioPresentationLatency`,
`IConnectionPoint`.

AU: a C shim for v2 and an Objective-C appex for v3 built with `xcodebuild`.

---

## Everything wrong, fragile or incomplete, and the proper fix

Severity: **S1** wrong behaviour in hosts or data loss; **S2** real-time,
safety or scaling hazard; **S3** missing capability a modern host expects;
**S4** maintainability.

### A. Format bindings and ABI

| # | Sev | Problem | Proper solution |
|---|---|---|---|
| A1 | S1 | VST3 is a hand-written C++ COM implementation with hand-typed IIDs, result codes and vtable offsets (`qVtbl[5]`). The changelog already lists fixed wrong IIDs and wrong `tresult` codes. Every new interface means hand-writing another vtable. | Build on the official MIT SDK. Either the `vst3` crate (generated from the SDK headers, implement interfaces in Rust, no C++) or vendored `pluginterfaces` with real C++ classes. Recommended: `vst3` crate, pinned, with its SDK commit recorded. Delete `vst3_shim.cpp`. |
| A2 | S2 | 512 parameter points per block in a fixed stack array; excess dropped silently. 32 channels per direction. A global descriptor allows one class per binary. | Read `IParamValueQueue` lazily during the block-splitting walk instead of copying into a fixed array: merge the queues by offset with a small cursor heap sized at activate. Size channel scratch from `bus_layouts()` at activate. Factory holds a slice of classes. |
| A3 | S2 | CLAP extension vtables, thread rules and host pointers are handled through raw `*const clap_host` in a 7.6k-line file. Thread correctness depends on comments. | Move to `clack-plugin` + `clack-extensions`, or reproduce its model inside MOOSE: a `Shared` part (any thread), a `MainThread` part, and an `AudioProcessor` value that `activate` creates and `deactivate` takes back. The audio state then belongs to the audio side by ownership, not by a flag. Wrap host extensions in thread-typed handles. |
| A4 | S3 | No `moduleinfo.json` (VST3 3.7.5+), so hosts must load the binary to scan. | Generate `Contents/Resources/moduleinfo.json` from `PluginInfo` and the param catalog in `cargo moose build`, and run the SDK's `moduleinfotool` validation in CI. |
| A5 | S3 | `clap-sys` 0.5 is from January 2025 and predates the 1.2.x drafts. | Track CLAP headers directly (bindgen against a pinned `free-audio/clap` tag) or move to clack. Keep draft extensions behind a `clap-draft` feature. |
| A6 | S4 | AU v2 is a 2.4k-line C shim; AUv3 needs `xcodebuild`. | Keep AU (no MIT alternative is better), but generate it against Apple's AudioUnitSDK (Apache-2.0) rather than a custom C shim, and make the AUv3 appex a thin wrapper over the same Rust core. Lower priority than A1. |

### B. Lifecycle and threading model

| # | Sev | Problem | Proper solution |
|---|---|---|---|
| B1 | S1 | `PluginCell` is an atomic flag that panics on overlap. A host that overlaps calls (FL Studio, Ardour, some Cubase paths) gets the call silently dropped. | Ownership, not a flag: the audio processor value exists only between activate and deactivate (A3). Host-thread callbacks never touch it. Where a host may call a lifecycle method during processing, queue it to the audio side, never assert. |
| B2 | S1 | One `reset` hook serves activate (may allocate), CLAP `reset` (audio thread) and VST3 `setProcessing(false)` (may be audio thread). | Three hooks with typed contracts: `activate(&mut self, AudioConfig) -> Result<AudioProcessor>` (main thread, may allocate, fallible), `reset(&mut AudioProcessor)` (audio thread, no allocation, checked by `rt-paranoid`), `deactivate(AudioProcessor)` (main thread, drops). The fallible-preparation machinery (`PreparationConfig`, `PreparationFrameBound`, `requires_fallible_preparation`) collapses into `activate` returning `Result`. |
| B3 | S2 | Stateless descriptor + `DspState` + shared `Params` + shell + `PluginRuntime` + `PluginLogicCore` + two leaf traits + two `Pure*` traits, generated by macros. Seven types for one plugin. | One trait with three associated types: `Shared` (params and atomics), `MainThread` (editor, presets, host-thread state), `Processor` (DSP). Precision is a type parameter on the processor, not two macro-stamped traits. |
| B4 | S2 | Panic in `process` is caught but not latched; `eprintln!` on the audio thread. | Latch the processor as failed; output silence; report once on the main thread (step 4 of the fix plan). |
| B5 | S3 | No use of host `thread-check`, `log`, `timer-support`, `posix-fd`, `thread-pool`. The task pool and GUI create their own threads. | Use host timers and fds for GUI ticks on Linux where available, host `log` for diagnostics, `thread-check` in debug assertions, and offer CLAP `thread-pool` as a backend for the task system. |
| B6 | S4 | Background tasks, snapshot publishers, persist signals, presentation revisions and edit epochs are separate mechanisms with their own atomics. | One main-thread message queue from audio and tasks to the host thread (bounded, preallocated), drained on `on_main_thread` / the VST3 timer. Every "tell the host later" goes through it. |

### C. State

| # | Sev | Problem | Proper solution |
|---|---|---|---|
| C1 | S1 | Save reads an audio-thread snapshot, so it can be stale; loads are queued and report success early. | Save and load on the main thread from the `MainThread` + `Shared` parts. DSP-only state that must be captured is requested from the processor through a bounded handshake (fix plan steps 1 and 2). |
| C2 | S1 | No CLAP `state-context`, so presets, duplicates and project saves are serialized the same way. | Implement `state-context` and pass a `StateContext { Preset, Duplicate, Project }` to save and load. Same for VST3 via `IStreamAttributes` (preset vs project) where the host provides it. |
| C3 | S3 | The envelope is custom (`plugin_id_hash`, params by numeric id, `extra`, `persist`), with `migrate_state` bolted on. | A versioned, self-describing envelope: params keyed by stable string ids (CLAP has `cookie` and string names; VST3 now has `IRemapParamID`), schema version, a migration chain `fn(v) -> v+1`. Keep reading old envelopes. |
| C4 | S3 | No undo integration. | CLAP `undo` draft behind `clap-draft`; VST3 `IComponentHandler2::startGroupEdit` / `finishGroupEdit` for grouped edits. |
| C5 | S3 | No `setDirty` for custom-state edits, so hosts don't mark the project modified. | Emit VST3 `IComponentHandler2::setDirty` and CLAP `state.mark_dirty` whenever custom or persisted state changes. |

### D. Parameters

| # | Sev | Problem | Proper solution |
|---|---|---|---|
| D1 | S1 | Parameters are a compile-time table of `&'static ParamInfo` with `&'static str` names. "Dynamic" means hiding or renaming static entries (`parameter_presentation`) or appending a static projection. A plugin cannot add, remove or retype parameters at runtime (modular synths, macro slots, per-instance routing). | A runtime `ParamCatalog` owned by `Shared`, built at instantiation from the derive plus any runtime additions, with owned strings. Structural changes go through a main-thread transaction: build a new catalog, swap the `Arc`, then notify the host (CLAP `params.rescan(CLAP_PARAM_RESCAN_ALL)` while deactivated, `RESCAN_INFO` / `RESCAN_TEXT` while active; VST3 `restartComponent(kParamTitlesChanged / kReloadComponent)`). The derive becomes one producer of the catalog, not the definition of it. |
| D2 | S1 | VST3 never sends `kParamValuesChanged`, so editor-driven preset loads and internal value changes leave host automation lanes and generic UIs stale. CLAP rescans, VST3 doesn't. | Send `kParamValuesChanged` from the main-thread queue after any non-host change of more than a few params; `performEdit` with gestures for single user edits. |
| D3 | S1 | CLAP modulation for `MODULATABLE` params is passed through but never applied. | Every float param carries a modulation offset (mono) applied before smoothing, cleared on transport reset as CLAP specifies. Poly modulation stays a per-voice event. VST3 has no mono-mod equivalent; leave it at zero there. |
| D4 | S2 | Smoother state is atomics inside the shared `Params`, stepped per sample; `f64` reads go through `f32`; per-sample and block APIs disagree. | Smoothers move into the processor (plain values, no atomics), generic over `f32`/`f64`, with one stepping rule shared by all APIs. `Shared` keeps only the atomic target. Matches JUCE's `SmoothedValue`; removes the per-sample atomic cost. |
| D5 | S2 | Every write bumps a process-wide `EDIT_EPOCH` shared by all instances. | Per-instance epoch only. Remove the global counter. |
| D6 | S2 | Each param write takes `normalize` → `denormalize` round trips and several dispatches by id. | Catalog lookup by dense index (CLAP `cookie` = index pointer; VST3 id → index via a perfect-hash or sorted table built at catalog swap). |
| D7 | S3 | Missing host-facing parameter features: CLAP `param-indication` (show host mapping/automation colour in the GUI), `context-menu` (host items in the plugin's right-click menu), draft `param-origin`, `param-hovered`; VST3 `IParameterFunctionName` (e.g. "dry/wet" for host macros), `IParameterFinder` (host "learn" by mouse position), `IComponentHandler3` context menu, `IMidiLearn2` / `IMidiMapping2`, `IRemapParamID`. | Add each as an optional capability on the catalog entry or the editor (e.g. `ParamInfo::function: Option<ParamFunction>`, `Editor::param_at(x, y)`, `Editor::context_menu(param)`), with wrappers mapping to whatever the format offers. |
| D8 | S3 | Value-to-text and text-to-value are per-param closures; no custom value lists exposed to hosts beyond enums. | Keep formatters, plus a `ValueList` kind (labelled discrete values, possibly runtime-populated, like a sample slot list) that maps to CLAP enum params with `value_to_text`, VST3 `kIsList` + `getParamStringByValue`, and AU value strings. Changing the list is a catalog transaction (D1). |
| D9 | S4 | ids: 24-bit nested-id domain, rebasing, `AUTO_PARAM_ID_MASK`, projection id ranges. | Stable string ids (`"osc1.cutoff"`) beside the numeric host ids, collision-checked when the catalog is built; arrays and nested groups compose string paths. State keys by string (C3). **Revised 2026-11-19:** numeric host ids are *not* hashed from string ids; see [D9 decision](#d9-string-ids-sit-beside-numeric-host-ids-decision-2026-11-19). |

### E. Events, MIDI and processing

| # | Sev | Problem | Proper solution |
|---|---|---|---|
| E1 | S2 | `EventList` is 2.6k lines with five parallel index vectors (order, exact links, companions, sequence) and O(n) `skip()`; AU output encoding is O(n²). | One sorted `Vec<Event>` per block with the exact MIDI 2 payload stored inline (enum), iterated by slice index. Merge host events and param points with a k-way cursor. Encode output once per block. |
| E2 | S3 | Block splitting is on, minimum 32 samples, for "chunked" params only; VST3 ramps become steps. | Keep splitting but let the processor choose: `Automation::BlockSplit { min }` or `Automation::PerSample` (deliver the point list and let the plugin ramp). Expose VST3 ramp segments as linear ramps, not steps. |
| E3 | S3 | No CLAP `note-name`, `voice-info`, `track-info`; no VST3 `IInfoListener`, `IKeyswitchController`, `INoteExpressionPhysicalUIMapping`, `IProgramListData`. | Add as optional `MainThread` capabilities: `note_names()`, `voice_info()`, `track_info_changed(TrackInfo)` (name, colour, channel count; maps to both CLAP and `IInfoListener`), keyswitches, program lists. |
| E4 | S3 | Audio ports are a static list of layouts. No CLAP `audio-ports-activation`, `configurable-audio-ports`, `ambisonic`; no VST3 `kIoChanged`. | A runtime port model on `MainThread`: the plugin can accept a host-proposed configuration (configurable-audio-ports / `setBusArrangements`), learn which buses are active (`audio-ports-activation` / `activateBus`), and request a change (`kIoChanged` / `audio_ports.rescan`). |
| E5 | S3 | Latency changes go through a VST3 shim timer and CLAP `request_callback`; no VST3 `IAudioPresentationLatency`. | Latency is set only during `activate` or via a main-thread request that triggers host restart (both formats require this); one code path for both. |

### F. GUI and host integration

| # | Sev | Problem | Proper solution |
|---|---|---|---|
| F1 | S3 | No Wayland (VST3 3.8 `IWaylandHost`, CLAP `gui` has Wayland only via floating windows). | Add `kPlatformTypeWaylandSurfaceID` support behind a feature once MUI has a Wayland backend. |
| F2 | S3 | Editor gestures, values and host notifications are format-specific code in each wrapper. | One `HostContext` handed to the editor: `begin_edit`, `set`, `end_edit`, `request_resize`, `mark_dirty`, `request_restart`, `context_menu`. Wrappers implement it once each. |
| F3 | S4 | `__moose_screenshot` and the hot-reload ABI are exported from release binaries. | Feature-gated (fix plan step 6). |

### G. Code quality

| # | Sev | Problem | Proper solution |
|---|---|---|---|
| G1 | S4 | Format wrappers are single files of 4k to 7.6k lines; tests live inside them. | Split per extension or interface (`params.rs`, `state.rs`, `gui.rs`, `ports.rs`, `process.rs`). Tests in `tests/` against a host harness (clack-host for CLAP, the SDK's validator for VST3). |
| G2 | S4 | Comments often restate history ("the old shim had…", "used to…") and run longer than the code. | Comments say what holds now and why; history goes to the changelog. |
| G3 | S4 | `unsafe impl Send` where auto traits suffice; `SendPtr<T>` is `Send + Sync` for any `T`. | Remove the unnecessary impls; bound `SendPtr<T: Sync>` or replace it with typed handles. |
| G4 | S4 | No conformance runs in CI. | CI runs `clap-validator`, the VST3 SDK `validator`, `pluginval` and `auval` on the examples for every PR. |

---

## Dynamic parameters: what each format allows, and the fallback

| Format | Rename, hide, regroup | Change text or values | Add, remove, retype |
|---|---|---|---|
| CLAP | `rescan(RESCAN_INFO)` while active (name, module, `is_hidden`, `is_periodic`) | `RESCAN_TEXT`, `RESCAN_VALUES` while active | Spec'd. While active: `request_restart()`, wait for `deactivate`, apply the change, `params.clear(id, CLEAR_ALL)` for removed or reused ids, then `rescan(RESCAN_ALL)`. The same path covers changing range, stepped, bypass, modulatable or cookie. |
| VST3 | `restartComponent(kParamTitlesChanged)` | `kParamValuesChanged` | Only through `kReloadComponent`: the host tears down and reloads the component, then re-reads `getParameterCount`. Host support varies (unverified per host), so treat it as best effort. |
| AU v2 / v3 | Parameter-list property change (v2); replacing the `AUParameterTree` (v3) | value strings | Possible in Logic, inconsistent elsewhere (unverified per host). |

**Fallback for formats or hosts that cannot add parameters: a slot pool.**
The plugin declares a fixed number of generic host parameters ("Slot 1" to
"Slot N", stable ids, never reused). At runtime the catalog binds each slot
to a real target, renames it through the "rename" column above (supported
everywhere), hides unused slots, and carries the binding in state so
automation keeps pointing at the same target after reload.

MOOSE already has pieces of this: array params (`[FloatParam; 8]`), the
presentation hook (rename, hide, regroup), and projections. What is missing
is one API on top: the plugin describes the parameters it wants, and each
wrapper reports its capability (`Full` for CLAP, `Reload` for VST3 hosts
known to handle `kReloadComponent`, `SlotPool` otherwise) and maps the request
onto it. Plugin code never branches on format.

## Target architecture

```
moose (facade)
  Plugin trait: type Shared, type MainThread, type Processor<S: Sample>
  ParamCatalog (runtime, Arc-swapped), HostContext, StateContext
moose-core
  catalog, events (single sorted Vec), state envelope + migrations,
  main-thread message queue, rt-paranoid
moose-clap   -> clack-plugin + clack-extensions (or pinned clap headers)
moose-vst3   -> vst3 crate (generated from the MIT SDK), moduleinfo.json
moose-au     -> AudioUnitSDK-based v2, thin AUv3 appex
moose-standalone, moose-loader (hot reload over the same Plugin trait)
```

Ownership replaces "trust the host": `activate` returns the `Processor`,
only the audio thread holds it, `deactivate` takes it back. The `Shared`
part holds atomics (param targets, meters, transport). The `MainThread` part
holds the catalog, editor, presets and custom state, and is the only thing
state save and load touch.

## Order of work (after fix plan steps 1 to 6)

1. **VST3 on the official SDK (A1, A2, A4, D2).** Highest risk reduction.
   Start with the `vst3` crate in a new `moose-vst3` that keeps the current
   Rust-side callbacks, then delete the shim. Validate with the SDK validator
   and pluginval.
2. **Lifecycle ownership model (B1, B2, B3).** The `Shared` / `MainThread` /
   `Processor` split; old traits stay as a compatibility layer for one
   release.
3. **Runtime parameter catalog (D1, D4, D5, D6, D8, D9) and modulation
   (D3).**
4. **CLAP on clack or current headers (A3, A5), with state-context (C2),
   param-indication, context-menu, track-info, note-name, voice-info (D7,
   E3).**
5. **Events rewrite (E1, E2) and ports model (E4, E5).**
6. **Host integration polish (B5, B6, C4, C5, F1, F2) and CI conformance
   (G4).**

Each phase lands with every plugin under `plugins/` migrated and the gate
(`tools/verify.sh`) green.

## CLAP: clack or our own wrappers (evaluation, 2026-11-18)

Decision for now: **keep moose-clap's own wrappers over `clap-sys`**, report
CLAP 1.2.10, and generate the 1.2.10 draft extensions with bindgen
(`moose_clap::draft`, `clap-draft` feature, `crates/moose-clap/tools/gen-draft.sh`).
Revisit clack when the lifecycle model (order of work, step 2) lands.

What clack (`clack-plugin` + `clack-extensions` 0.2) would give us:
- Thread-typed plugin parts (`Shared`, `MainThread`, `AudioProcessor`)
  and host handles that can only be used from the right thread. That is
  finding A3 and the model step 2 asks for.
- Extension vtables and the entry/factory boilerplate written once and
  tested upstream, instead of the hand-built `ClapExtensions` statics.
- Safe stream, event and port types.

Why not now:
- **It is the lifecycle rewrite in disguise.** clack fixes the plugin's
  shape (`Plugin::Shared`, `Plugin::MainThread`, `AudioProcessor`
  created in `activate`). Mapping moose's `PluginExport` onto that means
  doing order-of-work step 2 inside the CLAP wrapper only, and the brief
  rules out changing the plugin trait in this step. Done after step 2,
  the mapping is close to one to one.
- **Our extension surface is wider than its typed set.** moose-clap
  serves remote-controls, preset-load and preset-discovery,
  audio-ports-config, surround, note-ports, render, state-context and
  custom extensions from `P::clap_extension`. Each one clack lacks
  needs a raw escape hatch, which is what we have today.
- **Version lag is the same problem either way.** clack 0.2 also
  depends on generated headers; tracking a pinned free-audio tag
  ourselves (stable ABI unchanged since 1.2.2, drafts via bindgen) keeps
  the version under our control.
- **Risk.** moose-clap is 8.7k lines with host-specific workarounds and
  tests (Bitwig, REAPER, Live). A port now, with no clap-validator in CI,
  would trade known behaviour for unknown.

What we take from it now: the thread model is the target for step 2.
Host extension pointers should become thread-typed handles when
moose-clap is split per extension (G1).

## D9: string IDs sit beside numeric host IDs (decision, 2026-11-19)

The first D9 proposal hashed each stable string ID into the 32-bit host ID.
We are not doing that. **Numeric host IDs stay exactly as declared** (explicit
`id = N`, ordinal, or the field-path hash scheme, rebased by nesting), and a
plugin that ships keeps them forever.

Why: hosts store automation lanes, MIDI learn, remote-control pages and
generic-UI bindings by the numeric ID (CLAP `clap_id`, VST3 `ParamID`, AU
`AudioUnitParameterID`). Re-deriving those IDs from string IDs would give
every existing parameter a new number, so every saved project would lose its
automation and mappings on upgrade. A hash also cannot be renamed without
changing the number, which is the opposite of what a stable string ID is for.

What string IDs do instead:

- **Side map in `ParamCatalog`.** `string_id(id)`, `id_by_string(key)` and
  `by_string_id(key)` look them up; the derive fills them from field paths
  (`osc1.cutoff`, `bands[2]`) or `#[param(key = "...")]`.
- **OAST v2 state envelope.** When `state_schema_version() >= 1`, session
  state *and presets* are written as v2: each entry carries its string ID, its
  numeric fallback ID and the value, plus the plugin's schema version. On
  load, `StateMigrationChain` runs first (`v -> v + 1`), then
  `resolve_named_params` maps string IDs to the current numeric IDs, so a
  renumbered parameter still gets its saved value. Plugins on schema `0`
  (`kontra`, `kurv`) keep writing byte-identical v1, and every load path
  still reads v1.
- **One path for everything.** Host state (`parse_or_migrate`,
  `read_and_apply_params`), `restore_plugin` and preset loads share
  `migrate_to_current`; preset writes go through `serialize_preset` /
  `save_user_preset` (`serialize_state_for_schema` underneath), preset reads
  through `load_preset` / `load_preset_file` (`prepare_preset`).
- **VST3 `IRemapParamID`** remaps a host's stale numeric ID through the same
  catalog when a project names an ID this build no longer has.

Collisions never panic a host:

- **Compile time.** The derive rejects duplicate numeric IDs, duplicate own
  string IDs, and an own string ID inside a `#[nested]` field's `prefix.`
  namespace.
- **Construction time.** `Params::assert_no_id_collisions` (run by the
  derived `new()`) builds the catalog strictly and panics on a collision
  across nested structs, before the plugin reaches a host.
- **Runtime.** Catalogs built from runtime data (dynamic slots, string IDs
  read back from a file or host) use `ParamCatalog::try_new` /
  `try_with_string_ids`, which return a `CatalogError` (duplicate ID,
  duplicate string ID, one ID given two string IDs, a string ID naming no
  descriptor). The default `Params::param_catalog()` uses
  `with_string_ids_lossy`, which keeps the first claim and drops the
  conflicting string ID, so a bad runtime table degrades string lookup
  instead of panicking inside a host callback. `ParamCatalog::new` /
  `with_string_ids` keep panicking and are for tables already checked at
  compile time.

`moose_params::hash_param_id` still exists for a parameter set that has
never shipped, but moose never calls it to assign host IDs.

## Implementation Status (Updated 2026-11-21)

The structural overhaul tracked above is now implemented across `moose-core`, `moose-params`, `moose-clap`, `moose-vst3`, and `moose-au`:

- **A. Bindings & ABI:** `A1` (pure-Rust `vst3` 0.3.0 COM layer replacing C++ shim), `A2` (k-way `IParamValueQueue` merge + unbounded channel scratch), `A3` (phase-gated `HostHandle` + `SendPtr` removal), `A4` (`moduleinfo.json` + validation), `A5` (`clap-draft` feature for CLAP 1.2+ drafts).
- **B. Lifecycle & Threading:** `B1`/`B2` (`PluginCell`, `prepare`/`reset`, zero-polling event-driven Win32/Linux/Apple host wakes), `B4` (`PanicLatch` + `clap.log` forwarding), `B5` (`clap.log`, `clap.thread-check`, `clap.thread-pool` via `ProcessContext::parallel_for`), `B6` (single per-instance `HostQueue` + `HostSink` + `dispatch()` with `loom` proofs).
- **C. State:** `C1`/`C2` (`clap.state-context`, main-thread save/load), `C3` (OAST v2 string-ID envelope + `StateMigrationChain` + VST3 `IRemapParamID`), `C4` (VST3 `IComponentHandler2` group edits + `clap.undo/4` & `clap.undo_context/4` under `clap-draft`), `C5` (`setDirty` / `state.mark_dirty`).
- **D. Parameters:** `D1` (`DynamicParams<N>` runtime slot pool + `CatalogSignal`), `D2` (`kParamValuesChanged`), `D3` (CLAP mono modulation), `D4` (processor-owned generic smoothers), `D5` (per-instance edit epoch), `D6` (dense-index CLAP strict-provenance cookies + VST3 sorted ID→index lookups), `D7` (`param-indication`, `context-menu`, `IParameterFunctionName`, `IParameterFinder` + `MuiEditor::param_at`, `IMidiLearn2`/`IMidiMapping2`, `clap.params-origin/1`, `clap.param-hovered/1`), `D8` (`ValueListParam`), `D9` (side-map string IDs beside numeric host IDs).
- **E. Events, Audio & Host Interfaces:** `E1`/`E2` (single-vec `EventList`, `PerSample` ramps), `E3`/`E5` (`IAudioPresentationLatency`, `INoteExpressionPhysicalUIMapping`, `IKeyswitchController`, `IProgramListData`, `IPluginCompatibility`, `clap.ambisonic/3`).

## Sources

- CLAP changelog: https://github.com/free-audio/clap/blob/main/ChangeLog.md
- VST 3.8.0 release notes: https://steinbergmedia.github.io/vst3_dev_portal/pages/Versions/Version+3.8.0.html
- VST 3 MIT announcement: https://sonicstate.com/news/2025/10/30/vst-3-now-available-under-mit-license/
- VST 3.8.1: https://forums.steinberg.net/t/vst-3-8-1-sdk-released/1038865
- vst3-rs: https://github.com/coupler-rs/vst3-rs
- clack: https://github.com/prokopyl/clack
