# AU and AAX runtime and packaging

The AU v14 tables and `create_with_sink` constructor stay fixed. Loaders reject mismatched versions before accessing callback tails and honor complete fields in short sinks.

- AU render producers publish queue bits and mark a shared timer dirty using atomics. Control producers retain their per-instance dispatch source. Deactivation schedules a final main-queue drain after quiescence, outside lifecycle locks. An intervening reset preserves pending delay notifications.
- Parameter enumeration and tree rebuilds consume one exposed generation, including choices. Render adopts a live catalog between blocks and control reclaims retired catalogs.
- AUv2 supports both listener-removal selectors and returns `kAudio_MemFullError` at capacity. A concurrent remover can release its render callback refcon after removal returns.
- AUv3 publishes fully wired parameter trees, suppresses only matching synchronous feedback on the calling thread, and applies owned context names on main. Main-thread hooks connect before state restore.
- AU ramps sample each block-split boundary at its start, preserve the per-sample terminal store convention, and retain the interpolated continuation across slices. New events at the same frame override an earlier ramp. AUv2 render-thread writes retain their offsets.
<!-- coderabbit-ai-generated:v1 -->

<!-- coderabbit-ai-generated-section:v1:847767393b0548d13275e3c106cf0d66697a0b73749d85514a5574d90306d431 -->

- AAX submits prepared latency on initialization and retries failed host submissions on timer wake. Packaging honors the requested architectures and uses the static MSVC runtime by default. Set `MOOSE_MSVC_CRT=MD` to use the dynamic runtime when building the Rust archive, SDK, and adapter. `cargo moose install --aax --no-build` installs the existing bundle without rebuilding any part of it or changing its CRT. Build that bundle with the intended CRT first; its Rust archive must use the same CRT as the SDK and adapter. The archive cache keys on the SDK content, toolchain, configuration, architecture slices, CRT, and deployment target.

<!-- /coderabbit-ai-generated:v1 -->

Regression coverage uses the AU C-shim harness, Rust timing and queue tests, and AAX runtime tests. Apple and Windows SDK integration requires the corresponding host toolchains. Scope is limited to Moose and the shared AU and AAX v14 layouts remain fixed.
