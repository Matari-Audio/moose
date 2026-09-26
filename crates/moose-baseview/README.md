# moose-baseview

MOOSE's vendored copy of [baseview](https://github.com/RustAudio/baseview), the
low-level windowing library for audio plugin UIs.

- Upstream: `baseview` **0.3.4** from crates.io, git commit
  `bc987b96c22cbdb36507196dbbcad00c768e135b` (the `.cargo_vcs_info.json` of the
  published crate). The first commit that adds this directory is that source
  unchanged except for the package name and the removal of upstream's
  `[workspace]` table (its examples are not vendored).
- License: unchanged, MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).
- The public API stays source compatible with upstream 0.3.4. MOOSE additions
  are additive only. Depend on it under the upstream name so `use baseview::*`
  keeps working:

  ```toml
  baseview = { package = "moose-baseview", path = "crates/moose-baseview" }
  ```

## MOOSE additions (vs upstream 0.3.4)

(filled in by the following commits)
