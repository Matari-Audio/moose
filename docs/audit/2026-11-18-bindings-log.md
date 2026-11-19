# Bindings log (2026-11-18)

Work log for the "bindings" agent: moving the MOOSE format layer onto
official bindings (redesign doc section A, work item 1; audit findings 9
and 11). The decisions other agents depend on are in
`coordination/moose-overhaul/DECISIONS.md`. Cargo commands below ran from
`frameworks/moose` through `build-slot`.

## Step 1: VST3 onto the `vst3` crate

**Changed**
- `crates/moose-vst3/shim/vst3_shim.cpp` (3634 lines of hand-typed IIDs,
  result codes and vtables) is deleted. The COM objects are Rust on
  `vst3` 0.3.0 (coupler-rs bindings generated from the SDK 3.8.0
  `pluginterfaces`), in `crates/moose-vst3/src/com/`:
  - `mod.rs`: registration, the unit tree, string helpers, the
    `IBStream` state readers, and the instance registry
    (`Mutex<Vec<(ctx, Weak<Host>)>>`, which replaces the shim's
    64-slot map and its limit).
  - `component.rs`: one object implementing `IComponent`,
    `IAudioProcessor`, `IEditController`, `IEditControllerHostEditing`,
    `IMidiMapping`, `IMidiMapping2`, `IProcessContextRequirements`,
    `IUnitInfo` and `INoteExpressionController`. It also holds the
    off-UI-thread edit queue and the restart / persist notifications.
  - `view.rs`: `IPlugView` and `IPlugViewContentScaleSupport`.
  - `factory.rs`: `IPluginFactory3`.
  - `wake.rs`: per-OS UI-thread wakes. Linux uses `IRunLoop` with an
    eventfd plus a 30 ms timer, macOS a libdispatch main-queue source,
    Windows a thread timer.
- `build.rs` no longer compiles C++. It keeps only the MSVC
  `+crt-static` warning. The `cc` build dependency is gone; `libc`
  (Linux only) is new.
- `ffi.rs` keeps its structs, and its former `extern "C"` imports are
  plain Rust fns under the same names. `Vst3ParamChange` is gone.
  `process` and `process_f64` share one `ProcessFn` signature.
- **512-point parameter cap removed.** The COM layer walks every
  `IParamValueQueue` point and calls the new
  `Vst3Callbacks::push_param_change` once per point. The plugin's event
  lists are sized `EVENT_LIST_PREALLOC + 2 * param_count` at create.
  Input events are pushed first, so automation can only use the room
  notes leave. On overflow, the queue's last value is committed again
  after the block, so a final value is never lost; only intermediate
  ramp points past capacity are dropped. A cursor merge into the
  chunker is deferred to the events rewrite (E1).
- **32-channel cap removed.** Channel pointer scratch is sized from
  `max_layout_channels(bus_layouts())` (new
  `Vst3PluginDescriptor::max_inputs`), and the registration rejection
  is gone. Silence and trash buffers are sized in `setupProcessing` from
  the bus arrangement.
- `mark_restart` now works from any thread. It is lock-free inside
  `process`, and other threads go through the registry and deliver at
  once on the UI thread. Previously, a call made outside `process` was
  dropped.
- Behaviour kept from the shim:
  - bus info and names
  - activateBus masks, and setBusArrangements through
    `match_bus_layout_perbus`
  - the suspended-flush shape, the parameter commit rules, the
    transport flags, and the flattening gather with silence/trash
    scratch
  - in-to-out copy, event conversion both ways, output params, and
    clearing silence flags
  - the `param_presentation` flags, the `%.2f` fallback, and the
    performEdit echo guard
  - the handler swap protocol with gesture closing, the
    `kParamTitlesChanged` revision, and the editor queue budgets
    (drain 64, transition 4096)

**Differences from the shim, each forced by `vst3` QueryInterface
being static per class**
- `IUnitInfo` and `INoteExpressionController` are always exposed. With
  no MIDI input bus they answer one root unit and zero expressions,
  which is the same as not being there. This is logged in DECISIONS.md.
- The view holds the instance (`Arc<Host>`), not a COM reference to the
  component. Releasing the component while a view is open drops the
  component's count to 0, and the instance lives on until the view
  goes.

**Tests**
- The six C++ fixtures under `tests/native/` included the shim source,
  so they are deleted. They are ported to `src/com/native_tests.rs` and
  drive the COM objects directly with a fake callback table:
  - lifecycle
  - editor_attach
  - editor_lifetime
  - editor_threads (Linux only, with a fake `IRunLoop`)
  - parameter_units (5004 params, 197 units)
  - output_silence
- Exact COM refcount checks became held-vs-released checks.
- The eventfd-closed `fcntl` check was dropped: parallel tests can
  reuse the fd number at once, which made it flaky. The unregister is
  still asserted.

**Merging the fixes PR (#23)**
- `IAudioProcessor::setProcessing` calls fixes' `set_processing`
  callback; `false` resets through it, as their shim line did.
- `apply_host_state` keeps the `ParamsBackup` capture and its 0 return
  on a rejected `load_state` (lib.rs, untouched by the port).
- The queued-state apply in `process_block` is fixes'
  `apply_queued_state`; `kParamValuesChanged` is flagged when it applied
  successfully.

**Commands**
- VST3 SDK validator, built from vst3sdk `v3.8.0_build_66`
  (`cmake --build build --target validator`), on `Moose Synth.vst3` from
  `cargo moose build -p moose-example-synth --vst3 --clap`: 47 passed, 0
  failed. The bundle's `moduleinfo.json` matches the factory.
- pluginval and clap-validator: not installed here, not run.
- `build-slot cargo test -p moose-vst3`: 45 lib tests pass (fixture
  ports included); integration and doc tests also pass.
- `build-slot cargo clippy -p moose-vst3 --all-targets --target
  {x86_64-unknown-linux-gnu, x86_64-pc-windows-gnu,
  aarch64-apple-darwin}`: clean.

## Step 2: VST3 standards work

- **`restartComponent(kParamValuesChanged)`** is sent after a host state
  load (`apply_host_state`) and after the audio thread applies a queued
  state. Parameter edits from the editor still go through
  `performEdit`.
- **`IComponentHandler2::setDirty(true)`** comes from the new
  `ffi::moose_vst3_mark_dirty`. It is queued like edit gestures and
  delivered on the UI thread. The editor's custom-state `set_state`
  calls it. gui's `PluginContext::with_mark_dirty` will call it too
  (see DECISIONS ANSWER to gui).
- **Group edits (`startGroupEdit` / `finishGroupEdit`): skipped.** Moose
  has no API that groups parameter edits (redesign C4, undo), so there
  would be no caller. Add them with the C4 API.
- **gui requests**:
  - `gui_open` returns i32, and `attached` fails with kResultFalse
    (detaching) when the editor didn't open.
  - `onFocus` returns kResultOk.
  - `setContentScaleFactor` returns kResultFalse on macOS.
- **`IMidiMapping2`**: MIDI 1 input assignments are listed from the same
  callback `getMidiControllerAssignment` answers (buses x 16 channels x
  controllers 0..=130). There are no MIDI 2 assignments.
- **`IMidiLearn` / `IMidiLearn2`: skipped.** Moose has no MIDI-learn
  concept to forward live controller input to.
- **`IParameterFunctionName`: skipped.** `ParamInfo` has no function
  role. The one role flag, `IS_BYPASS`, has no `FunctionNameType`
  entry.
- **`IInfoListener`: skipped.** There is no core API to hand track
  name or colour to a plugin. The CLAP `track-info` work needs the same
  API, so both would come with it.
- **`kReloadComponent` for dynamic parameters: skipped.** Moose has no
  dynamic parameter set yet (the runtime catalog is order-of-work step 3).
  The one runtime change that exists, presentation (names/flags), already
  sends `kParamTitlesChanged` through `param_presentation_revision`.
  `kReloadComponent` (or the slot-pool fallback) goes with that catalog.
- **`moduleinfo.json`**: `cargo-moose` loads the built module, reads its
  `IPluginFactory2` like the SDK's `moduleinfotool`, and writes
  `Contents/Resources/moduleinfo.json`. This happens in
  `cargo moose build` / `package` staging and in `install`, before
  codesigning on macOS. Cross-arch builds skip it with a log line,
  because the binary can't be loaded and the file is optional.

## Step 3: CLAP 1.2.10 (branch `bindings/clap-1.2.10`)

- Upstream 1.2.3 to 1.2.10 changed no stable ABI: they added draft
  extensions, docs, the UIKit GUI API string and two surround channel
  constants. So `clap-sys` 0.5 stays for stable types, and
  `moose_clap::CLAP_VERSION` reports 1.2.10.
- Draft extensions and factories are generated by bindgen from the
  pinned `1.2.10` tag (`tools/gen-draft.sh`). They are exposed as
  `moose_clap::draft` behind the `clap-draft` feature, which `moose`
  forwards. Nothing uses them yet.
- `state-context` is served. Every context saves and loads the full
  state, because moose state has no preset-only or project-only parts.
- Skipped, because moose has no concept for them:
  - `param-indication` (no mapping or automation indicator in the editor
    API)
  - `context-menu`
  - `track-info` (needs the same core track-info API as VST3
    `IInfoListener`)
  - `note-name`
  - `voice-info` (the plugin API reports no voice count or capacity)
- The clack evaluation is in the redesign doc ("CLAP: clack or our own
  wrappers"). Moose stays on its own wrappers until the lifecycle model
  (step 2) exists.
- Commands: `build-slot cargo clippy -p moose-clap --all-targets
  --features clap-draft` (clean), `build-slot cargo test -p moose-clap`
  (46 passed).

## Step 4: VST3 on the runtime catalog (branch `bindings/vst3-host-features`)

Stacked on the runtime-catalog core (`moose/runtime-catalog`).

- **Catalog wake.** `Host::new` binds a `CatalogSignal` through the new
  `set_catalog_waker` callback. Its wake only sets `catalog_pending` and
  schedules the UI-thread wake (the same eventfd / main-queue / thread-timer
  path as `mark_restart`). The UI-thread flush takes the changes once:
  VALUES → `kParamValuesChanged`, INFO → `kParamTitlesChanged`, STRUCTURE →
  see below. Changes made during construction are discarded, because the
  host hasn't read the list yet.
- **No presentation polling.** `param_presentation_revision` is read only
  after an event: a processed or host-set change to a
  `parameter_affects_presentation` id (audio thread included; it sets a flag
  and wakes, like CLAP's `request_changed_presentation`), an editor
  `performEdit`, any `mark_restart` (state loads), or a catalog wake.
- **Dynamic capability.** `DynamicSupport::{Reload, SlotPool}` is chosen in
  `initialize` from `IHostApplication::getName`. Reload is used for hosts whose
  name contains "Cubase" or "Nuendo" (unverified). Reload enumerates only
  `presentation.available` parameters through an index table, rebuilt on the
  UI thread when STRUCTURE is taken or the revision changes. STRUCTURE sends
  `kReloadComponent`. SlotPool lists every slot; free ones are `kIsHidden`,
  read-only and not automatable, and STRUCTURE sends `kParamTitlesChanged |
  kParamValuesChanged`. Param ids never change, and the processor accepts
  every id. `param_presentation` now returns hidden / unavailable bits.
- **`IParameterFunctionName`**: matches `Params::parameter_function` mapped to
  the SDK `FunctionNameType` strings (all seven `ParamFunction` roles); a
  non-root unit restricts the search to that unit.
- **`IInfoListener`**: channel name (UTF-16) and colour (ARGB) reach
  `MainThreadHooks::track_info_changed`. The SDK channel context has no
  channel count or track kind, so those stay unset.
- **`IMidiLearn` / `IMidiLearn2`**: MIDI 1 controllers (CC 0..=127,
  kAfterTouch, kPitchBend, kCtrlProgramChange) reach
  `MainThreadHooks::midi_learn`. kResultTrue is returned when it binds the
  controller. MIDI 2 (RPN/NRPN) input returns kResultFalse because
  `MidiSource` has no such variant.
- The hooks live in a `MainThreadCell` on `Vst3Instance`. `MainThread` is
  minted only in the IInfoListener / IMidiLearn callbacks.
- **Group edits**: `PluginContext::begin_group` / `end_group` (DECISION in
  DECISIONS.md, additive) queue `EditOp::Group` alongside the gestures, so
  `startGroupEdit` / `finishGroupEdit` reach the host on the UI thread in
  order with the edits. Other formats: no-op.
- Skipped: Wayland (a separate item).

## Step 5: CLAP on the runtime catalog (branch `bindings/clap-host-features`)

- `crates/moose-clap/src/catalog_sync.rs` owns parameter publishing.
  `init` binds a `CatalogSignal` whose wake only calls `request_callback`,
  coalesced on the same flag as presentation refreshes. `on_main_thread`
  and `deactivate` run `sync_parameters`, which also covers the old
  presentation-revision and value-rescan paths:
  - `VALUES`: `RESCAN_VALUES`. `INFO` (or a new presentation revision):
    `RESCAN_INFO | RESCAN_TEXT`.
  - `STRUCTURE` while active: one `request_restart`. The removed ids
    collect in `pending_structure`, and the published list stays as it is.
  - Inactive (at once, or in `deactivate`): rebuild the catalog with
    `param_catalog()`, swap it into both owners, `params.clear(id,
    CLEAR_ALL)` for every removed id, then `RESCAN_ALL`.
- Catalog ownership: the main-thread list (`ExposedParams`, catalog and
  visible indices in one value behind the existing mutex) and the audio
  thread's `Arc<ParamCatalog>` in the audio cell. Both are replaced only
  while the instance is inactive, when no `process` or audio-thread `flush`
  can hold the cell. `process` reads its own `Arc` and takes no lock.
- `MainThreadHooks` from `PluginExport::main_thread()` live in a
  `MainThreadCell`. A `MainThread` token is minted only inside
  `[main-thread]` callbacks (init, deactivate, on_main_thread, and the new
  extension entry points). Re-entrant host calls get the hooks' default
  instead of panicking (`try_borrow_mut`).
- Extensions, one module each: `track_info.rs` (read once at init if the
  host has it, then on `changed`), `note_name.rs` (`count` takes a fresh
  list, `get` reads it), `voice_info.rs`, `param_indication.rs` and
  `context_menu.rs`. clap-sys 0.5 has all five stable structs, so no
  local bindings were needed.
- voice-info rule: `get_extension` is `[thread-safe]` and can't call the
  hooks, so `init` asks `voice_info()` once and the extension is offered
  only if that returned `Some`. A later `None` makes `get` return false.
- Skipped: a plugin-side way to call `clap_host_note_name.changed` and
  `clap_host_voice_info.changed`. That needs a main-thread request channel
  from the plugin, which `PluginContext` doesn't have yet. Follow-up.
- Tests: `src/host_features_tests.rs` uses a recording host and recording
  hooks over a `DynamicParams<4>` plugin.
- Commands: `build-slot cargo clippy -p moose-clap --all-targets -- -D
  warnings` (clean), `build-slot cargo test -p moose-clap` (57 passed).
## Step 6: lifecycle trait (branch `moose/lifecycle-trait`)

- Redesign B2/B3, additive. `moose_plugin::Plugin<S: Sample = f32>`
  (`crates/moose-plugin/src/lifecycle.rs`), re-exported as `moose::Plugin`
  and from every prelude. Three associated types: `Shared: Params`,
  `MainThread: MainThreadHooks`, `Processor: Send + 'static`.
- Precision is the trait parameter, not a GAT: `impl Plugin for X` is f32,
  `impl Plugin<f64>` (or `Plugin<Sample>` under `prelude64`) is f64. A GAT
  `Processor<S>` would make every author write DSP generic over `S` while
  the prelude still picks one precision at compile time.
- `activate` takes `&Shared`, not `&Arc<Shared>`: the leaf `prepare` hook
  it runs from only has `&Params`, and `process` receives `&Shared` every
  block, so a processor never needs to hold the `Arc`.
- `Lifecycle<P>` is a marker type implementing `PluginLogic` for
  `P: Plugin<f32>` and `PluginLogic64` for `P: Plugin<f64>`; no overlap
  with the `PurePluginLogic` blanket because no crate can implement
  `PurePluginLogic` for `Lifecycle<_>`. Its `DspState` is
  `ProcessorSlot<P::Processor>` (inactive with an optional pending state
  load, or active). `prepare` deactivates the old processor, carries its
  `save_state` bytes (or a load that arrived while inactive) into the new
  one, and leaves the slot inactive on error. `process` while inactive
  writes silence and returns `Normal`; `reset` while inactive is a no-op;
  `load_state` while inactive runs `validate_state` and keeps the bytes.
- `moose::plugin!` still declares `pub type Plugin` in the caller module,
  which shadows the prelude trait there, so the example writes
  `impl moose::Plugin for Bitcrusher`.
- Not carried (stay on `PluginLogic` for these): `InitContext` /
  background tasks, `snapshot_into` / `snapshot_version`,
  `supports_in_place`, `clap_extension`, `state_changed`.
- Example: `moose-example-dasp-bitcrusher` migrated.
- Tests: `crates/moose-loader-tests/tests/lifecycle_test.rs` (5 tests:
  silence before activate, activate error is a preparation failure,
  reset/process reach the processor, state loaded before activation and
  across re-activation, main-thread hooks returned).

## Validators (2026-11-19, main 542bd8c1f6)

Fresh `cargo moose build -p moose-example-synth --vst3 --clap`:
- clap-validator 0.4.1: 44 run, 41 passed, 0 failed, 3 skipped (no input
  ports, no audio-ports-activation, no configurable-audio-ports).
- VST3 SDK validator 3.8.0: 47 passed, 0 failed.
- pluginval (official Linux release, Dec 2024), VST3,
  `--validate-in-process --skip-gui-tests`: SUCCESS at strictness 5 and 10.

