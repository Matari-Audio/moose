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
