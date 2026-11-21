# {project_title}

Audio plugin{{ if is_workspace }} workspace{{ endif }} built with [moose](https://github.com/Matari-Audio/moose).
{{ if is_workspace }}
## Plugins
{{ for p in plugin_crates }}
- `{p}`
{{- endfor }}
{{ endif }}
## Development

```sh
{{ if has_standalone -}}
# Fast-compile standalone launch (no DAW required)
cargo moose run --debug

{{ endif -}}
# Build & install into per-user plugin directories (release by default)
cargo moose install --clap
cargo moose install --vst3
cargo moose install --au2        # macOS only

# Bundle every enabled format into target/bundles/
cargo moose build

# Run tests & verify zero audio-thread heap allocations
cargo test --workspace
{{- for p in plugin_crates }}
cargo test -p {p} --features rt-paranoid
{{- endfor }}

# Validate installed plugins (clap-validator, pluginval, auval)
cargo moose validate

# Package signed installers into target/dist/
cargo moose package
```

## Fast Local Builds

`Cargo.toml` configures `[profile.dev]` with `debug = "line-tables-only"` and `[profile.dev.package."*"]` with `opt-level = 1` / `debug = 0`: your own crates rebuild quickly, and dependencies are lightly optimized so DSP and GUI stay responsive in `--debug` builds (at the cost of a slightly slower first build).

`cargo moose` `--debug` builds, `cargo test`, `cargo clippy` and rust-analyzer share one target cache, so switching between them doesn't rebuild dependencies. Release builds via `cargo moose` add `-C target-cpu=x86-64-v3` on x86_64.

On x86_64 Linux, Rust 1.90+ already links with the bundled `rust-lld`. On Windows, uncomment the `lld-link` snippet in `.cargo/config.toml` for faster linking.
{{ if has_ci }}
## CI

`.github/workflows/ci.yml` runs formatting, Clippy, cross-platform tests (Linux, macOS, Windows) with `cargo-nextest` and `rt-paranoid`, and release builds on every branch push and pull request.
{{ endif }}
