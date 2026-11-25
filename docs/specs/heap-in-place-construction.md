---
status: implemented
project: frameworks/moose
owner: antigravity
created: 2026-11-24
updated: 2026-11-24
proof_scope: internal
evidence_freshness_days: 180
---

# Moose Heap-In-Place Construction, Stack-Budget Guard & Off-Thread Persist Decode

## Problem & Goal

- **Problem:** Rust lacks placement `new`: `Box::new(T::default())` and `Arc::new(T::default())` construct `T` on the caller's stack before moving it to the heap, relying on unguaranteed LLVM copy-elision that breaks across dev/release profiles and Cargo feature unification. Large DSP processors and parameter stores can silently blow past 1 MiB (Windows), 512 KiB (macOS AU/secondary), or 128 KiB (Linux musl) DAW host thread stacks even when the top-level struct is boxed, because nested field initializers inline into one frame. Additionally, `moose::Plugin` (`Lifecycle<P>`) lacked an off-thread decode hook after `#[persist]` fields load so plugins could build derived DSP tables heap-in-place off-thread and swap them RT-safely on the audio thread.
- **Goal:** Provide zero-stack-temporary heap-in-place construction primitives (`moose_dsp::HeapArray`, `moose::HeapInit`, `#[derive(HeapInit)]`, `#[params(heap_init)]`), boxed plugin lifecycle construction/teardown (`Plugin::{create_boxed, activate_boxed, deactivate_boxed}`, `PluginLogicCore::init_boxed`, `Params::new_arc`), off-thread `#[persist]` decode (`Plugin::{decode_persist_state, apply_decoded_persist}`), a 256 KiB lifecycle stack-budget test helper (`moose_test::assert_lifecycle_fits`), and 64 KiB Clippy stack-budget lints (`large_stack_arrays`, `large_stack_frames`, `large_types_passed_by_value`).
- **Plan Reference:** Moose heap-in-place construction + stack-budget guard (follow-up enabler for plugin stack-budget migrations).

## Primary-Source & Version Proof

N/A — internal refactor: relies exclusively on Rust standard library (`Box::new_uninit`, `Arc::new_uninit`, `core::ptr::addr_of_mut!`, `std::thread::Builder::stack_size`) and internal Moose crates (`moose-dsp`, `moose-core`, `moose-params`, `moose-derive`, `moose-plugin`, `moose-loader`, `moose-test`, `moose`), with zero new external crate dependencies or host-SDK ABI changes.

## Pinned Crate & SDK Ground Truth

| Dependency / Tool | Pinned Version | Lockfile / Header Source | Verified Symbols / Features |
|---|---|---|---|
| `syn` | `2.0.118` | `frameworks/moose/Cargo.lock` | `DeriveInput`, `Data::Struct`, `Fields`, `Attribute::parse_nested_meta`, `Type::Path` (`moose-derive`) |
| `quote` | `1.0.46` | `frameworks/moose/Cargo.lock` | `quote!`, `format_ident!` (`moose-derive`) |
| `proc-macro2` | `1.0.106` | `frameworks/moose/Cargo.lock` | `Span`, `TokenStream` (`moose-derive`) |

## Invariants & Non-Goals

### Hard Invariants
- **Audio RT-Safety:** `Plugin::decode_persist_state` runs strictly on the host/main thread; `Plugin::apply_decoded_persist` runs on the audio thread inside `RtSection` and retires replaced payloads via `StateLoadQueue` off-thread retirement with zero audio-thread allocations or deallocations (verified under `--features rt-paranoid`).
- **Zero-Stack-Temporary Construction & Unwind Safety:** `HeapArray<T, N>` (`#![forbid(unsafe_code)]`), `HeapInit::heap_init`, `HeapInit::heap_init_arc`, and `#[derive(HeapInit)]` never materialize large arrays or structs on the stack, verify struct-field completeness at compile time, and drop every already-initialized field on panic unwind before freeing `Box<MaybeUninit<Self>>` / `Arc<MaybeUninit<Self>>`.
- **Safe-by-Default `#[derive(Params)]`:** Default `#[derive(Params)]` emits zero `unsafe` so `#![forbid(unsafe_code)]` crates keep compiling; large parameter stores opt in via `#[params(heap_init)]`.
- **Backwards Compatibility & Dependency Graph Hygiene:** Existing `Plugin`, `PluginLogicCore`, `Params`, and `PluginRuntime` implementations compile unchanged. Neither `moose-core` nor `moose` adds a dependency on `moose-dsp`, keeping downstream lockfiles untouched.
- **Standalone / Mirror Boundaries:** Zero references to monorepo-only paths, private tooling, or local filesystem paths inside `frameworks/moose/`.

### Non-Goals
- No edits to downstream plugin crates in this change (plugins migrate independently in follow-up PRs).
- No nightly-only Rust placement-in-place language features; autoref specialization works on stable Rust.

## API / Data / File Contract

- **`moose-dsp`:**
  - `moose_dsp::HeapArray<T, const N: usize>` (`from_fn`, `splat`, `from_boxed_array`, `into_boxed_array`, `into_boxed_slice`, `as_slice`, `as_mut_slice`, `iter`, `iter_mut`, `Deref`/`DerefMut` to `[T; N]`, `Default`, `Clone`, `Index`/`IndexMut`, `AsRef`/`AsMut`, `IntoIterator`).
- **`moose-core` / `moose`:**
  - `moose::HeapInit` (`unsafe fn heap_init_in_place(dst: *mut Self)`, `fn heap_init() -> Box<Self>`, `fn heap_init_arc() -> Arc<Self>`), `moose::heap_box_array_init::<T, N>() -> Box<[T; N]>`.
  - `StateField` impls for `Box<T>` and `Box<[T; N]>`.
  - `PluginExport::decode_persist_state(&Params, StateContext) -> Option<DecodedState>`.
  - `PluginRuntime::apply_decoded_persist(&mut self, DecodedState, StateContext) -> Option<DecodedState>`.
  - `QueuedLoad::decoded_persist` + `StateLoadQueue::push_with_backup_decoded_persist_and_context` wired through `restore_plugin`, `apply_or_queue_state_with_context`, and `apply_queued_state`.
- **`moose-params` / `moose-derive`:**
  - `Params::new_arc() -> Arc<Self>` (default `Arc::new(Self::default())`).
  - `#[derive(HeapInit)]` with field attributes `#[heap_init(with = expr)]`, `#[heap_init(default)]`, `#[heap_init(some)]`, plus generics, `#[cfg]`, tuple/unit structs, and `Option<Box<T>>`.
  - `#[params(heap_init)]` struct attribute on `#[derive(Params)]` generating `HeapInit` + `Params::new_arc()`.
- **`moose-plugin` / `moose-loader`:**
  - `Plugin::{create_boxed, activate_boxed, deactivate_boxed, decode_persist_state, apply_decoded_persist}` with backwards-compatible defaults (including default `Plugin::activate` returning `Err(PreparationError::NotReady)` for plugins overriding `activate_boxed`).
  - `PluginLogicCore::{init_boxed, decode_persist_state, apply_decoded_persist}`, wired in `Lifecycle<P>`, `StaticShell`, and `moose::plugin!`.
- **`moose-test`:**
  - `moose_test::assert_lifecycle_fits::<P: PluginExport>(kib: usize)` running create -> prepare/activate -> process -> save/load -> re-prepare/deactivate -> drop on a `kib * 1024`-byte thread stack.
- **Workspace Lints:**
  - `Cargo.toml` `[workspace.lints.clippy]`: `large_stack_arrays = "deny"`, `large_stack_frames = "deny"`, `large_types_passed_by_value = "warn"`.
  - `clippy.toml`: `array-size-threshold = 65536`, `stack-size-threshold = 65536`.

## Verification Matrix

| Check | Tier | Target / OS | Exact Command | Pass Criterion | Status / Observed Evidence |
|---|---|---|---|---|---|
| Spec proof gate | Doc | linux-x86_64 | `python3 tools/check-spec-proof.py frameworks/moose/docs/specs/heap-in-place-construction.md` | Exit 0, zero proof errors | PASS (2026-11-24, linux-x86_64, commit 29ea1df1d) |
| Moose workspace unit & integration tests (incl. 128 KiB / 256 KiB stack tests) | Compile/ABI | linux-x86_64 | `build-slot cargo nextest run --manifest-path frameworks/moose/Cargo.toml --workspace` | Exit 0, all tests pass | PASS (2026-11-24, linux-x86_64, commit 29ea1df1d) |
| Stack-budget lifecycle guard in dev + `--release` | Validator/Runtime | linux-x86_64 | `build-slot cargo nextest run --manifest-path frameworks/moose/Cargo.toml --workspace --release --no-tests=fail -E 'test(/::lifecycle_fits_stack_budget$/)'` | Exit 0, all 5 examples pass at 256 KiB | PASS (2026-11-24, linux-x86_64, commit 29ea1df1d) |
| Clippy with 64 KiB stack-budget lints | Compile/ABI | linux-x86_64 | `build-slot cargo clippy --manifest-path frameworks/moose/Cargo.toml --workspace --all-targets -- -D warnings` | Exit 0, zero warnings | PASS (2026-11-24, linux-x86_64, commit 29ea1df1d) |
| RT-safety paranoid audit (incl. off-thread persist decode RT swap) | Validator/Runtime | linux-x86_64 | `bash frameworks/moose/scripts/ci/rt-paranoid.sh` | Exit 0, zero RT allocations/deallocations | PASS (2026-11-24, linux-x86_64, commit 29ea1df1d) |

## Unverified Boundaries & Promotion Gate

None — all verification tiers execute on the current host.

## Acceptance Criteria

- [x] `moose_dsp::HeapArray<T, const N: usize>` constructs large arrays on the heap without stack materialization (`from_fn`, `splat`, `Deref`/`DerefMut` to `[T; N]`).
- [x] `#[derive(HeapInit)]` initializes fields in place via `Box::new_uninit()` + `addr_of_mut!`, recurses into nested `HeapInit` fields, enforces field completeness, is unwind-safe, and supports generics, `#[cfg]`, `#[heap_init(with = ...)]`, and `Option<Box<T>>`.
- [x] `Plugin::{create_boxed, activate_boxed, deactivate_boxed}`, `PluginLogicCore::init_boxed`, and `Params::new_arc` (`#[params(heap_init)]`) keep processor/params lifecycle off the stack.
- [x] `Plugin::{decode_persist_state, apply_decoded_persist}` decode persisted state off-thread and swap derived tables RT-safely on the audio thread with zero allocations/deallocations under `rt-paranoid`.
- [x] `moose_test::assert_lifecycle_fits::<P>(256)` passes across all example plugins and a >512 KiB synthetic plugin in both dev and `--release`.
- [x] 64 KiB Clippy stack-budget lints and `clippy.toml` thresholds pass cleanly across the workspace.

## Task Decomposition

| # | Subtask | Owned Paths | Depends On | Verification |
|---|---|---|---|---|
| 1 | `HeapArray` & `HeapInit` trait | `crates/moose-dsp`, `crates/moose-core` | — | `cargo nextest run -p moose-dsp -p moose-core` |
| 2 | `#[derive(HeapInit)]` & `#[params(heap_init)]` | `crates/moose-derive`, `crates/moose-params` | 1 | `cargo nextest run -p moose --test heap_init` |
| 3 | Boxed lifecycle & off-thread persist decode | `crates/moose-core`, `crates/moose-plugin`, `crates/moose-loader`, `crates/moose` | 1, 2 | `cargo nextest run --workspace` + `rt-paranoid.sh` |
| 4 | `assert_lifecycle_fits` & 64 KiB clippy guard | `crates/moose-test`, `examples/*`, `Cargo.toml`, `clippy.toml` | 3 | `cargo clippy --workspace --all-targets -- -D warnings` + `--release` lifecycle tests |

## Rollout & Affected Dependents

- **Affected Projects:** `frameworks/moose` (direct); all Moose-backed plugins are compatible with zero required changes and opt into the 64 KiB stack-budget gate incrementally as they migrate.
- **Mirror Impact:** Copybara PR to `Matari-Audio/moose`; zero private or monorepo paths inside `frameworks/moose/`.
- **Versioning & Changelog:** Minor additive API within `moose` 7.x; existing plugins require no breaking changes.
