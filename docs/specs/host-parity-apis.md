---
status: in-progress
project: moose
owner: claude
created: 2026-11-24
updated: 2026-11-25
proof_scope: external-api
evidence_freshness_days: 180
---

# Host identity, process waker and standalone host control

## Problem & Goal

- **Problem:** Plugins migrating onto moose need three things the SDK did not expose:
  - which host is loading them;
  - a safe way to ask the host to call `process` again (the plugin may be idle);
  - control over the standalone runner from an embedding application, meaning the audio API, live device, rate, buffer and layout changes, and an open result.
- **Goal:**
  1. Every wrapper hands the plugin a `HostIdentity` at creation, in `init`, `activate` and `prepare`.
  2. CLAP plugins get a `ProcessWaker` that cannot reach a freed or half-created host.
  3. `moose-standalone` exposes `run_with_args`, `on_audio_opened`, `reopen_audio` and a `jack` feature. Each audio request is validated up front and applied transactionally on one owner thread.
- **Plan Reference:** moose rows of `docs/STACK-OWNERSHIP-PLAN.md` (host-integration parity).

## Primary-Source & Version Proof

| Standard / Vendor API | Pinned Version / Spec Rev | Primary-Source URL | Verified Date | Key Invariants / Deprecations Checked |
|---|---|---|---|---|
| CLAP `clap_host` (`name`, `version`, `request_process`) | CLAP 1.2 via clap-sys 0.5.0 | https://github.com/free-audio/clap/blob/main/include/clap/host.h | 2026-11-24 | `request_process` is `[thread-safe]`; host callbacks are forbidden during `create_plugin`; a failed create returns null and `destroy` is never called |
| VST3 `IHostApplication::getName` | VST 3 SDK via vst3 0.3.0 | https://steinbergmedia.github.io/vst3_doc/vstinterfaces/classSteinberg_1_1Vst_1_1IHostApplication.html | 2026-11-24 | Context arrives in `IPluginFactory3::setHostContext` / `IComponent::initialize`; name is a `String128` |
| cpal hosts (`available_hosts`, `host_from_id`) | cpal 0.17.1 | https://docs.rs/cpal/0.17.1/cpal/fn.host_from_id.html | 2026-11-24 | `host_from_id` fails with `HostUnavailable` (e.g. a stopped JACK server); `jack` is a cargo feature |

## Pinned Crate & SDK Ground Truth

| Dependency / Tool | Pinned Version | Lockfile / Header Source | Verified Symbols / Features |
|---|---|---|---|
| `clap-sys` | 0.5.0 | `frameworks/moose/Cargo.lock` | `clap_host.{name,version,request_process}` (src/host.rs) |
| `vst3` | 0.3.0 | `frameworks/moose/Cargo.lock` | `IHostApplication::getName`, `String128` (src/bindings.rs) |
| `cpal` | 0.17.1 | `frameworks/moose/Cargo.lock` | `available_hosts`, `host_from_id`, `HostId::name`, feature `jack` (src/platform/mod.rs) |

## Invariants & Non-Goals

### Hard Invariants
- **Audio RT-Safety:**
  - The standalone output callback's per-channel lanes and pointer arrays are preallocated at stream setup, sized from the negotiated frame bound and both bus widths, so the first block does not allocate.
  - `ProcessWaker::wake` is lock-free and allocation-free.
  - The render callback takes the plugin with a single `try_lock`, which never waits: on contention (an editor state read) the block outputs silence. This is the existing ownership model shared with the editor, and this change does not alter it.
- **Determinism & Compatibility:**
  - State formats are untouched.
  - Hot reload carries the instance context through every generation (`moose_init_state`) and passes the full `PreparationConfig`, including the host, to every generation (`moose_prepare_v2`, ABI epoch 13).
- **Mirror Boundaries:** All code and docs are generic moose APIs. No product names and no private paths.
- **Waker safety:**
  - The CLAP waker is inert until `clap_plugin.init` succeeds.
  - A waker invoked during `create`, or one that escaped a failed (null) creation, never calls the host.
  - `destroy` closes the gate and waits for wakes already in flight.
- **Transactional standalone control:**
  - One worker thread owns both cpal streams.
  - A request (launch, controller or menu change, `reopen_audio`) is validated before anything is applied. The validation covers the audio API, routes with channel `1..=4096`, sample rate, buffer, and bus layout.
  - The request is then committed completely, or the previous configuration is restored.
  - Each request yields exactly one `on_audio_opened` result. An explicitly requested input that fails makes the request fail.
  - Capture opens at the target layout's ring width and is rebuilt whenever that width changes. When the exact count isn't advertised, capture opens the narrowest advertised count above it at the render rate. JACK advertises 1/2/4/6/8/16/..., so a 15-input layout opens 16 ports and drops the extra lane. `AudioOpened.input_channels` reports the port count that actually opened.
  - `AudioOpened.api` uses the documented names. JACK is reported as `"JACK"`, although cpal's `HostId::name` returns `"Jack"`.
  - A panicking observer is contained.
  - Controllers report a stopped worker as `Err`.
  - Only one standalone session may run per process. A session's guard releases only its own registration.

### Non-Goals
- Host identity for AU/AAX beyond the process name. Those formats have no host-name API, so `HostIdentity::from_process` is used.
- Sample-rate conversion of capture devices that can't run at the render rate. These fall back to the device default and are not resampled, as before.
- Live sample-rate changes while `--input-file`, `--sidechain-file` or `--output-file` is in use. Such requests fail and are rolled back, because those streams are fixed to the launch format.

## API / Data / File Contract

- **Supported cases and deviations from the migration-bindings request.** This section amends GAP-1..3 of a host plugin's migration-bindings spec; where they differ, this contract wins:
  - **Spec:** `HostIdentity { format: HostFormat, name, version }` with a required identity.
    **Shipped:** `HostFormat` is `#[non_exhaustive]` (Clap/Vst3/Au/Aax/Lv2/Standalone). `InitContext::host()` and `PreparationConfig::host` are `Option`.
    **Why `Option`:** every wrapper and both shells (static and hot reload) always set it. `None` only occurs for an `InitContext` / `PreparationConfig` built outside a wrapper, such as test drivers or offline rendering. A required field would break every direct constructor.
  - **Spec:** `on_audio_opened` reports the API name.
    **Shipped:** it reports `Result<AudioOpened, String>`, which carries the API, output device, sample rate, buffer, output/input widths and bus layout of the committed configuration.
  - **Spec:** `select_audio_api(name)` process-global.
    **Shipped:** removed. The API belongs to the request (`--audio-api`, `reopen_audio`), so concurrent sessions can't clobber each other.
  - **Spec:** `InitContext::process_waker()`.
    **Shipped:** CLAP returns `Some`; the other formats return `None`, because they have no equivalent host call.
- **Public types:**
  - `moose_core::info::{HostFormat, HostIdentity}`;
  - `moose_core::main_thread::ProcessWaker`;
  - `moose_core::tasks::InstanceContext`;
  - `PluginExport::create_for(InstanceContext)` (default delegates to `create`);
  - `InitContext::{with_instance, set_host, host, process_waker}`;
  - `PreparationConfig::with_host`;
  - `moose_standalone::{run_with_args, on_audio_opened, reopen_audio, AudioOpened}`;
  - `audio::list_devices(api)`, `audio::MAX_ROUTE_CHANNEL`.
- **Breaking changes (rollout contract, moose Unreleased):**
  - `PreparationConfig` is `#[non_exhaustive]` and gains `host`. Build it with `PreparationConfig::new(..).with_host(..)`, not a struct literal.
  - `cli::Options` gains `audio_api`. Build it with `cli::parse_from`, not a struct literal.
  - `AudioHandles` holds a private session guard and can only be obtained from `start_audio`.
  - `InputController::{set_enabled, set_device}` and `OutputController::{set_device, set_layout}` return `Result<(), String>`.
  - `select_audio_api` is removed. `list_devices` takes the API.
  - An invalid `--bus-layout`, route, rate or buffer is now an error, not a warning with a fallback. An unsupported explicit `--sample-rate` fails instead of silently running at the device default.
  - `StaticShell::from_parts` takes a third `Option<InstanceContext>` argument.
  - Hot-reload dylibs must be rebuilt: the ABI epoch is 13, with `moose_init_state(.., Option<InstanceContext>)` and `moose_prepare_v2`.
- **CLI:**
  - `--audio-api <name>` / `MOOSE_STANDALONE_AUDIO_API`;
  - `--list-devices` lists the selected API's devices;
  - feature `jack`;
  - feature `rt-paranoid` for the standalone allocation audit.

## Verification Matrix

| Check | Tier | Target / OS | Exact Command | Pass Criterion | Status / Observed Evidence |
|---|---|---|---|---|---|
| Spec proof gate | Doc | any | `python3 tools/check-spec-proof.py frameworks/moose/docs/specs/host-parity-apis.md` | Exit 0, zero proof errors | PENDING |
| Core / wrapper unit tests | Compile/ABI | linux-x86_64 | `matari fast moose -p moose-core -p moose-clap -p moose-vst3 -p moose-plugin` | Exit 0, zero failures | PENDING |
| Loader entry points | Compile/ABI | linux-x86_64 | `matari fast moose -p moose-loader -p moose-loader-tests` | Exit 0, zero failures | PENDING |
| Standalone control tests | Validator/Runtime | linux-x86_64 | `build-slot cargo nextest run -p moose-standalone --features rt-paranoid,jack` | Exit 0; scratch audit reports zero allocations | PENDING |
| Live JACK 16-port switch | Hardware/OS-CI | linux-x86_64 with JACK | `reopen_audio(["app","--audio-api","jack","--bus-layout","<16-in index>"])` on a running standalone | `AudioOpened.input_channels == Some(16)` | PENDING_OS_CI (linux-jack) |

## Unverified Boundaries & Promotion Gate

- A live JACK server with 16 capture ports is not available in CI. Only the pure width planning and the request diffing are unit-tested; the hardware row stays `PENDING_OS_CI`.
- AU/AAX identity is unit-tested only. Apple and Windows host runs need their SDK toolchains.
