---
status: in-progress
project: frameworks/moose
owner: antigravity
created: 2026-11-24
updated: 2026-11-24
proof_scope: external-api
evidence_freshness_days: 180
---

# Moose Wave 4A: CLAP/VST3 Parity, Reactive `moose-mui` Bridge & Linux Symbol Hiding

## Problem & Goal

- **Problem:** Four concrete gaps remain after Moose Wave 3B before plugins achieve full CLAP/VST3 host parity and zero-leak Linux plugin binaries:
  1. `moose-vst3`'s `IMidiMapping2Trait` currently stubs `getNumMidi2ControllerAssignments` (`0`) and `getMidi2ControllerAssignments` (`kResultFalse`), so plugins cannot advertise MIDI 2.0 Registered/Assignable Controller assignments to VST3 SDK 3.8 hosts.
  2. `moose-clap`'s `clap.state-context/2` ignores the host's `context_type` (`CLAP_STATE_CONTEXT_FOR_PRESET` / `DUPLICATE` / `PROJECT`) and `moose-vst3`'s `IComponent::{getState,setState}` does not query `IStreamAttributes` (`PresetAttributes::kStateType` / `StateType::{kProject,kDefault,kTrackPreset}`), preventing plugins from discriminating project save/load vs. preset vs. duplicate/default state operations.
  3. `moose-mui::Bridge` does not expose host `ParamIndication` (automation state/color and controller mapping label/description/color) or live parameter modulation offsets (`CLAP_EVENT_PARAM_MOD` mono and poly summary), does not push coalesced host wakeups into MUI's 0 Hz wake loop, and does not automatically forward widget right-clicks (`HostContext::context_menu`) or hover transitions (`HostContext::param_hovered`).
  4. `cargo-moose` lacked an automated Linux `.clap` and `.vst3` dynamic export allowlist check (`nm -D --defined-only`) to guard against accidental `#[no_mangle]` / `#[export_name]` leaks in `.dynsym`.
- **Goal:** Deliver all four capabilities cleanly within the `moose` workspace, with zero new external crate dependencies, zero audio-thread RT-safety regressions (`rt-paranoid` clean, zero audio-thread `rt::allow_alloc` additions), full backwards compatibility for existing plugins, and empirical verification including `nm -D --defined-only` and `clap-validator`.
- **Plan Reference:** Moose Wave 4A — CLAP/VST3 feature parity, reactive `moose-mui` bridge, and Linux symbol hiding.

## Primary-Source & Version Proof

| Standard / Vendor API | Pinned Version / Spec Rev | Primary-Source URL | Verified Date | Key Invariants / Deprecations Checked |
|---|---|---|---|---|
| Steinberg VST3 SDK `IMidiMapping2` | VST3 SDK 3.8.0+ | https://github.com/steinbergmedia/vst3_pluginterfaces/blob/master/vst/ivstmidimapping2.h | 2026-11-24 | `IMidiMapping2` replaces `IMidiMapping` (`[UI-thread & Connected]`). `Midi2Controller` is 2 bytes (`byte1`: bits 0..6 bank, bit 7 registered; `byte2`: bits 0..6 index); `Midi2ControllerParamIDAssignment` is 12 bytes (`alignof = 4`: `ParamID pId`, `BusIndex busIndex`, `MidiChannel channel`, `Midi2Controller controller`). `getMidi2ControllerAssignments` fills host-allocated `list.map[0..list.count]` and returns `kResultTrue` (`kResultOk`) on success, `kInvalidArgument` on null list/map-with-nonzero-count, `kResultFalse` on unsupported direction. |
| Steinberg VST3 SDK `IStreamAttributes` & `StateType` | VST3 SDK 3.8.0+ | https://github.com/steinbergmedia/vst3_pluginterfaces/blob/master/vst/ivstattributes.h | 2026-11-24 | `IBStream` passed to `IComponent::{getState,setState}` may implement `IStreamAttributes` (queried via `queryInterface`); `getAttributes()` returns a borrowed `*mut IAttributeList` (no extra AddRef). `IAttributeList::getString(id, buf, sizeInBytes)` writes UTF-16 `TChar` into `buf` and returns `kResultTrue` when present. |
| Steinberg VST3 SDK `PresetAttributes` / `StateType` keys | VST3 SDK 3.8.0+ | https://github.com/steinbergmedia/vst3_pluginterfaces/blob/master/vst/vstpresetkeys.h | 2026-11-24 | `PresetAttributes::kStateType = "StateType"`. Canonical `StateType` constants are `kProject = "Project"`, `kDefault = "Default"`, and `kTrackPreset = "TrackPreset"` (note: SDK does not define `StateType::kPreset`; `"Preset"` is accepted defensively alongside `kTrackPreset`). |
| CLAP `clap.state-context/2` | CLAP 1.2.2+ | https://github.com/free-audio/clap/blob/main/include/clap/ext/state-context.h | 2026-11-24 | `CLAP_EXT_STATE_CONTEXT = "clap.state-context/2"`. `CLAP_STATE_CONTEXT_FOR_PRESET = 1`, `CLAP_STATE_CONTEXT_FOR_DUPLICATE = 2`, `CLAP_STATE_CONTEXT_FOR_PROJECT = 3`. Both `save` and `load` are `[main-thread]`; blobs saved under any context must remain loadable under plain `clap.state` or another context. |
| CLAP `clap.param-indication/4` | CLAP 1.2.2+ | https://github.com/free-audio/clap/blob/main/include/clap/ext/param-indication.h | 2026-11-24 | `CLAP_EXT_PARAM_INDICATION = "clap.param-indication/4"`. `set_mapping` and `set_automation` are independent `[main-thread]` callbacks per `param_id`; automation states are `NONE(0)`, `PRESENT(1)`, `PLAYING(2)`, `RECORDING(3)`, `OVERRIDING(4)`. |
| CLAP `clap.context-menu/1` | CLAP 1.2.2+ | https://github.com/free-audio/clap/blob/main/include/clap/ext/context-menu.h | 2026-11-24 | `CLAP_EXT_CONTEXT_MENU = "clap.context-menu/1"`. `clap_host_context_menu::popup(host, target, x, y)` opens host menu at logical editor coordinates for `CLAP_CONTEXT_MENU_TARGET_KIND_GLOBAL` or `CLAP_CONTEXT_MENU_TARGET_KIND_PARAM`. |
| GNU `ld` / `lld` / `mold` `--version-script` | GNU Binutils `ld` | https://sourceware.org/binutils/docs/ld/VERSION.html | 2026-11-24 | Anonymous ELF version script `{ global: sym1; sym2; local: *; };` exports listed dynamic symbols and forces all other symbols to local visibility. Compatible with GNU `ld`, LLVM `lld`, and `mold` via `-Wl,--version-script=<path>`. |

## Pinned Crate & SDK Ground Truth

| Dependency / Tool | Pinned Version | Lockfile / Header Source | Verified Symbols / Features |
|---|---|---|---|
| `vst3` | `0.3.0` | `frameworks/moose/Cargo.lock` | `IMidiMapping2Trait::{getNumMidi2ControllerAssignments,getMidi2ControllerAssignments}`, `Midi2Controller`, `Midi2ControllerParamIDAssignment`, `Midi2ControllerParamIDAssignmentList`, `IStreamAttributes`, `IStreamAttributesTrait::getAttributes`, `IAttributeList`, `IAttributeListTrait::getString`, `PresetAttributes::kStateType`, `StateType::{kDefault,kProject,kTrackPreset}`. Verified ABI quirk: `vst3 0.3.0` expands `Midi2Controller` bitfields into 4 `u8` fields (`sizeof(Midi2ControllerParamIDAssignment) == 16`), whereas C++ SDK 3.8.0/3.8.1 layout is 12 bytes (`pId`, `busIndex`, `channel`, `byte1`, `byte2` + 1 pad byte); `moose-vst3` writes exact 12-byte C++ ABI entries. |
| `clap-sys` | `0.5.0` | `frameworks/moose/Cargo.lock` | `CLAP_EXT_STATE_CONTEXT` (`"clap.state-context/2"`), `CLAP_STATE_CONTEXT_FOR_PRESET` (`1`), `CLAP_STATE_CONTEXT_FOR_DUPLICATE` (`2`), `CLAP_STATE_CONTEXT_FOR_PROJECT` (`3`), `clap_plugin_state_context`, `CLAP_EXT_PARAM_INDICATION`, `CLAP_PARAM_INDICATION_AUTOMATION_*`, `CLAP_EXT_CONTEXT_MENU`, `CLAP_EVENT_PARAM_MOD`. |
| `mui` | `0.4.2` | `frameworks/moose/Cargo.lock` | `Ui::get(&widget)` -> `Response { hovered, clicked_with(Button::Secondary), .. }`, `Ui::pointer().pos`, `Ui::edits_for`, `Id::of`, `El`, `IntoEl`. |
| `mui-baseview` | `0.4.2` | `frameworks/moose/Cargo.lock` | `Requests::redraw(&self)` (sets atomic flag + wakes 0 Hz idle loop), `EmbeddedWindow::dispatch` (already handles Wayland synchronized resize internally — no `set_sync` method exists or is added). |

## Invariants & Non-Goals

### Hard Invariants
- **Audio RT-Safety:** Zero heap allocations, zero deallocations, zero `rt::allow_alloc` escapes, zero mutex locks, zero blocking I/O, syscalls, or panics on steady-state audio processing blocks and contextual state saves. Audio-thread modulation/automation editor wakeups MUST use lock-free atomics (`HostQueue::signal(HostSignal::EDITOR_WAKE)` gated on `EditorFeedback::has_waker()`, plus pre-allocated single-`AtomicU64` packed `(min: f32, max: f32)` poly-modulation slots with `u64::MAX` empty sentinel) and pass `scripts/ci/rt-paranoid.sh`. `save_state_with_context` runs strictly on the host/main thread when stopped; while audio is active, contextual payloads are served from the off-thread `SnapshotPublisher::publish_for_context` lane (generation-gated by `load_gen`, never touched on the audio thread) with zero audio-thread locks or allocations.
- **Determinism & Compatibility:** Existing plugins implementing `Plugin` / `PluginLogicCore` / `PluginRuntime` / `MainThreadHooks` MUST compile and behave identically with zero required code changes; state envelopes remain wire-compatible across all `StateContext` values.
- **Standalone / Mirror Boundaries:** Zero hardcoded host or external repository paths may appear in code or docs.
- **C++ VST3 ABI Fidelity:** `IMidiMapping2::getMidi2ControllerAssignments` MUST write exact 12-byte Steinberg C++ `Midi2ControllerParamIDAssignment` entries via `write_unaligned` (`byte1` = `(bank & 0x7F) | (registered << 7)`, `byte2` = `index & 0x7F`), strictly bounded by `list.count`, return `kResultFalse` when fewer than `list.count` valid assignments exist, and reject invalid pointers or out-of-range controller fields (`bank > 127`, `index > 127`, `channel >= 16`, `bus_index < 0`).
- **Linux Dynamic Export Hygiene:** Empirically, `rustc`'s built-in `cdylib` version script already hides all internal Rust/dependency symbols across `ld.bfd`, `ld.lld`, and `mold` (only Moose's feature-gated `#[unsafe(no_mangle)]` format entry points appear in `.dynsym`). `cargo-moose` enforces this invariant mechanically on Linux builds via a post-link `nm -D --defined-only` allowlist guard (`clap_entry` for CLAP; `GetPluginFactory`, `ModuleEntry`, `ModuleExit` for VST3; plus any validated `moose.toml` `linux_extra_exports` and standard ELF linker symbols).

### Non-Goals
- No changes outside the `moose` workspace.
- No new external crate dependencies or lockfile churn outside the `moose` workspace.
- No invented Wayland `EmbeddedWindow::set_sync` API (already handled internally in `mui-baseview`).
- No Wave 4B validator CI workflow changes (deferred to Wave 4B).
- No duplication of already-implemented features (`CLAP_EVENT_PARAM_MOD` ingestion, `clap.thread-pool`, `HostContext::context_menu`, `HostContext::param_hovered`, `clap.param-indication` -> `MainThreadHooks::param_indication`).

## API / Data / File Contract

- **VST3 `IMidiMapping2` (`moose-core`, `moose-plugin`, `moose-vst3`, `moose`):**
  - `moose_core::main_thread::Midi2ControllerAssignment { pub param_id: u32, pub bus_index: i32, pub channel: u8, pub controller: Midi2Controller }` (re-exported in `moose::prelude`).
  - `MainThreadHooks::midi2_controller_assignments(&self, _cx: &MainThread, _direction: BusDirection) -> Vec<Midi2ControllerAssignment>` (default empty).
  - `Plugin::midi2_controller_assignments(_direction: BusDirection) -> Vec<Midi2ControllerAssignment>` (forwarded through `Lifecycle` -> `PluginLogicCore` -> `PluginExport`; default empty).
  - `Vst3Backend::midi2_controller_assignments(&self, _direction: BusDirection) -> Vec<Midi2ControllerAssignment>` (default empty; `Vst3Instance<P>` merges static `Plugin` + dynamic `MainThreadHooks` assignments, validating `bus_index >= 0`, `channel < 16`, `bank <= 127`, `index <= 127`).
  - `Component` (`IMidiMapping2Trait`): serves `getNumMidi2ControllerAssignments` and `getMidi2ControllerAssignments` for `BusDirections_::kInput` / `kOutput` using 12-byte `AbiMidi2Assignment` records.

- **Cross-Format `StateContext` (`moose-core`, `moose-plugin`, `moose-loader`, `moose-clap`, `moose-vst3`, `moose`):**
  - `moose_core::state::StateContext`: `#[non_exhaustive]` enum `{ Unknown, Preset, Project, Duplicate, CopyPaste, Default }` (`#[default] Unknown`, re-exported in `moose::prelude`).
  - Backwards-compatible default methods on `Plugin`, `PluginLogicCore`, and `PluginRuntime`:
    - `save_state_with_context(..., _context: StateContext) -> Option<Vec<u8>>` (host/main thread only; default `None` preserves the off-thread contextual snapshot / snapshot lane / `snapshot_into` / `save_state` fallback; returning `Some(Vec::new())` intentionally suppresses custom state for a context)
    - `SnapshotPublisher::publish_for_context` / `publish_for_context_for_generation` (off-thread contextual snapshot lane indexed by `StateContext`, generation-gated by `load_gen`, never touched on the audio thread so active-audio contextual saves perform zero audio-thread locks or allocations)
    - `load_state_with_context(..., data: &[u8], _context: StateContext) -> Result<(), StateLoadError>` (delegates to `load_state`; `Lifecycle` buffers pre-activation raw loads and replays them on `activate()`)
    - `validate_state_with_context(data: &[u8], _context: StateContext) -> Result<(), StateLoadError>` (delegates to `validate_state`)
    - `decode_state_with_context(data: &[u8], _context: StateContext) -> Result<Option<DecodedState>, StateLoadError>` (delegates to `decode_state`)
    - `apply_decoded_state_with_context(..., _context: StateContext)` (delegates to `apply_decoded_state`)
    - `state_changed_with_context(..., _context: StateContext)` (delegates to `state_changed`)
  - `moose-clap`: `state_context_save` / `state_context_load` map `CLAP_STATE_CONTEXT_FOR_{PRESET,DUPLICATE,PROJECT}` -> `StateContext::{Preset,Duplicate,Project}`; `preset_load_from_location` uses `StateContext::Preset`; plain `state_save` / `state_load` use `StateContext::Unknown`.
  - `moose-vst3`: `IComponent::{getState,setState}` queries `IBStream` for `IStreamAttributes`, reads `PresetAttributes::kStateType`, maps `kProject` -> `Project`, `kDefault` -> `Default`, `kTrackPreset` / `"Preset"` -> `Preset`, absent/other -> `Unknown`, and passes it to `Vst3Backend::state_{save,load}_stream_with_context`.

- **Reactive `moose-mui` Bridge (`moose-params`, `moose-derive`, `moose-core`, `moose-clap`, `moose-vst3`, `moose-mui`):**
  - `Params::get_modulation(&self, _id: u32) -> f64` (default `0.0`; implemented by `#[derive(Params)]` and `DynamicParams`).
  - `moose_core::main_thread::ParamIndicationState { pub automation: AutomationState, pub automation_color: Option<HostColor>, pub mapping: Option<HostMapping> }`.
  - `moose_core::editor::{ParamModulation, EditorFeedback}`: lock-free single-`AtomicU64` poly-mod summary per parameter (pre-allocated at instance creation, packing `(min: f32, max: f32)` with `u64::MAX` NaN-pair inactive sentinel for zero spin loops and zero torn reads; reset per active poly-mod block so live ranges shrink instead of latching session extremes, and cleared on reset/deactivate/stop/voice-silence), main-thread `ParamIndicationState` map with monotonic atomic revision, and coalesced editor wakeup hook.
  - `HostSignal::EDITOR_WAKE` in `moose_core::host_queue`: raised RT-safely once per audio block on `ParamChange` / `ParamMod`, drained on host main thread to trigger the registered editor waker (`MuiEditor` wires `Requests::redraw()` behind a `SeqCst` coalescing `AtomicBool` installed only after window creation succeeds and cleared on `close()` / `detach()`).
  - `HostContext` default methods: `param_indication(&self, id: u32) -> ParamIndicationState`, `param_indication_revision(&self) -> u64`, `param_modulation(&self, id: u32) -> ParamModulation`, `set_editor_waker(&self, waker: Option<Arc<dyn Fn() + Send + Sync>>)` (forwarded via `PluginContext`).
  - `moose_mui::Bridge`:
    - `indication(&self, id: impl Into<u32>) -> ParamIndicationState`
    - `modulation(&self, id: impl Into<u32>) -> f64`
    - `modulation_state(&self, id: impl Into<u32>) -> ParamModulation`
    - `modulated_value(&self, id: impl Into<u32>) -> f64`
    - `modulation_normalized(&self, id: impl Into<u32>) -> f64`
    - `changed(&mut self) -> bool` tracks parameter values, meters, mono/poly modulation offsets, and `param_indication_revision`.
    - Automatic transition-only `HostContext::param_hovered(Option<u32>)` dispatch across bound controls (and `None` on close).
    - Unified synchronous release-frame secondary-click dispatch in `finish_frame` across `context_menu_target` surfaces, bound parameter controls (`MenuTarget::Param(id)`), and background (`MenuTarget::Global`) with zero double-firing in 0 Hz idle mode.

- **`cargo-moose` Linux Dynamic Export Verification (`crates/cargo-moose`):**
  - `PluginDef`: optional `#[serde(default)] linux_extra_exports: Vec<String>` in `moose.toml`.
  - `build_format_dylibs` on Linux host targeting Linux (`target_os_of(...) == "linux"`) for `BuildFormat::Clap` and `BuildFormat::Vst3`: preserves batched `cargo build` (relying on `rustc`'s built-in `cdylib` version script) and runs `nm -D --defined-only` post-link to fail the build immediately if any required format entry point is missing or any unexpected dynamic symbol leaks outside `{clap_entry}` (CLAP) / `{GetPluginFactory, ModuleEntry, ModuleExit}` (VST3) + `linux_extra_exports` + standard ELF linker symbols.

## Verification Matrix

| Check | Tier | Target / OS | Exact Command | Pass Criterion | Status / Observed Evidence |
|---|---|---|---|---|---|
| Spec proof gate | Doc | any | `check-spec-proof.py docs/specs/wave-4a-parity-and-reactive-bridge.md` | Exit 0, zero proof errors | PASS (2026-11-24, linux-x86_64, PR #148; 0 errors / 0 warnings) |
| Formatting check | Compile/ABI | linux-x86_64 | `cargo fmt --all -- --check` | Exit 0, zero diffs | PASS (2026-11-24, linux-x86_64, PR #148; exit 0) |
| Workspace clippy | Compile/ABI | linux-x86_64 | `cargo clippy --workspace --all-targets -- -D warnings` | Exit 0, zero warnings | PASS (2026-11-24, linux-x86_64, PR #148; exit 0) |
| Workspace tests | Validator/Runtime | linux-x86_64 | `cargo nextest run --workspace` | Exit 0, all tests pass including new IMidiMapping2, StateContext, reactive Bridge, and Linux export allowlist tests | PASS (2026-11-24, linux-x86_64, PR #148; 1074 passed / 0 failed / 5 skipped) |
| RT-safety audit | Validator/Runtime | linux-x86_64 | `bash scripts/ci/rt-paranoid.sh` | Exit 0, zero RT allocation/lock violations | PASS (2026-11-24, linux-x86_64, PR #148; 55/55 tests passed across 4 example crates including active-audio contextual save RT check) |
| Cross-target compile (macOS & Windows) | Compile/ABI | aarch64-apple-darwin, x86_64-pc-windows-msvc | `cargo check --target aarch64-apple-darwin -p moose-vst3 -p moose-clap -p moose-mui -p moose-standalone --features moose-standalone/gui && cargo check --target x86_64-pc-windows-msvc -p moose-vst3 -p moose-clap -p moose-mui -p moose-standalone --features moose-standalone/gui` | Exit 0 on both cross targets | PASS (2026-11-24, linux-x86_64, PR #148; exit 0 on both cross targets) |
| Linux symbol hygiene & host load proof | Validator/Runtime | linux-x86_64 | `cargo-moose build --debug --clap --vst3 -p moose-example-gain-mui` + `nm -D --defined-only` + `clap-validator validate` | CLAP exports only `clap_entry`; VST3 exports only `GetPluginFactory`, `ModuleEntry`, `ModuleExit`; `cargo-moose` post-link `nm -D` guard passes; `clap-validator` passes | PASS (2026-11-24, linux-x86_64, PR #148; CLAP exports clap_entry only and VST3 exports GetPluginFactory + ModuleEntry + ModuleExit only; clap-validator 39 passed / 0 failed / 0 warnings / 5 skipped) |
| Live host proof (macOS & Windows) | Validator/Runtime | macOS AU/VST3/CLAP, Windows VST3/CLAP | Load `moose-example-gain-mui` in a native DAW; exercise IMidiMapping2, IStreamAttributes context menus and native windowing | Loads, round-trips state, menus and windowing behave | PENDING_OS_CI (needs native macOS/Windows runners or hardware) |

## Unverified Boundaries & Promotion Gate

- Live interactive DAW testing of `IMidiMapping2`, VST3 `IStreamAttributes` context menus, and native windowing on macOS (AU/VST3/CLAP) and Windows (VST3/CLAP) hosts cannot execute on this Linux x86_64 host; cross-target compilation (`aarch64-apple-darwin`, `x86_64-pc-windows-msvc`) and Linux runtime/ABI/validator suites execute locally.

**Hard Promotion Rules:**
1. This spec **MUST NOT** move to `status: implemented` while any required row in `## Verification Matrix` is `PENDING`, `PENDING_OS_CI`, or `BLOCKED`, or while any unchecked `- [ ]` acceptance criterion remains.
2. An unverified OS backend or platform path **MUST NOT** be flipped to default until its `PENDING_OS_CI` row becomes `PASS` on real OS CI or hardware.

## Task Decomposition

| # | Subtask | Owned Paths | Depends On | Verification |
|---|---|---|---|---|
| 1 | Write & validate Gate 0 Wave 4A SDD spec | `docs/specs/wave-4a-parity-and-reactive-bridge.md` | — | `check-spec-proof.py` exits 0 |
| 2 | Core types & reactive feedback (`StateContext`, `Midi2ControllerAssignment`, `ParamIndicationState`, `ParamModulation`, `EditorFeedback`, `HostSignal::EDITOR_WAKE`, `Params::get_modulation`) | `crates/moose-params/**`, `crates/moose-derive/**`, `crates/moose-core/**`, `crates/moose-plugin/**`, `crates/moose-loader/**`, `crates/moose/**` | 1 | `cargo nextest run -p moose-params -p moose-core -p moose-plugin` |
| 3 | CLAP `state-context` + `param-indication` feedback + modulation/editor wake | `crates/moose-clap/**` | 2 | `cargo nextest run -p moose-clap` |
| 4 | VST3 `IMidiMapping2` (12-byte C++ ABI) + `IStreamAttributes` `StateContext` + editor wake | `crates/moose-vst3/**` | 2 | `cargo nextest run -p moose-vst3` |
| 5 | Reactive `moose-mui::Bridge` (indications, modulation, coalesced 0 Hz wakeups, right-click context menu, hover transitions) | `crates/moose-mui/**` | 2 | `cargo nextest run -p moose-mui` |
| 6 | `cargo-moose` Linux `nm -D --defined-only` export allowlist verification + empirical `nm -D` & `clap-validator` proof | `crates/cargo-moose/**` | 1 | `cargo nextest run -p cargo-moose` + `nm -D --defined-only` + `clap-validator` |

## Rollout & Affected Dependents

- **Affected Projects:** Direct: `moose` workspace. Downstream plugins require zero source changes due to backwards-compatible defaults on all new trait methods.
- **Mirror Impact:** All paths and checks stay strictly inside the `moose` workspace with zero external repository references.
- **Versioning & Changelog:** Workspace remains at `7.0.0` during Wave 4A development; changelog entry will be added on release cut.
