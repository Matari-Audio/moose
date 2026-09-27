[package]
name = "{crate_name}"
{{ if is_workspace -}}
version.workspace = true
edition.workspace = true
{{- else -}}
version = "0.1.0"
edition = "2024"
{{- endif }}

[lib]
crate-type = ["cdylib", "staticlib", "rlib"]
{{ if has_standalone }}
[[bin]]
name = "{crate_name}-standalone"
path = "src/main.rs"
required-features = ["standalone"]
{{ endif }}
# Scaffolded default: {default_label}. AU builds only on macOS; on
# other hosts `cargo moose` skips it with a one-line note.
# Each format feature gates the matching wrapper crate as an optional
# dep.
[features]
default = {default_features | unescaped}
clap = ["dep:moose-clap", "dep:clap-sys"]
vst3 = ["dep:moose-vst3"]
au = ["dep:moose-au"]
{{ if has_standalone -}}
standalone = ["dep:moose-standalone"]
{{ endif -}}
shell = ["moose/shell"]
# Flags any allocation your DSP makes on the audio thread in `process`.
# Dev/test only: `cargo test --features rt-paranoid`. Off, zero-cost by
# default.
rt-paranoid = ["moose/rt-paranoid"]

[dependencies]
moose = \{ {dep_args | unescaped} }
# A MUI editor (`features = ["mui"]`): MUI takes moose-baseview from
# `git = "https://github.com/Matari-Audio/moose"` with no tag. To link one
# baseview, take every moose crate from that same unpinned git URL
# (Cargo.lock pins the commit), or `[patch."https://github.com/Matari-Audio/moose"]
# moose-baseview = \{ path = "<moose checkout>/crates/moose-baseview" }`.
# A tag, branch or rev on moose here is a second copy.
# Lightweight types for layout / theme / widget descriptions.
moose-gui-types = \{ {dep_args | unescaped} }
# Built-in renderer. Plugins that supply their own editor (egui)
# can drop this dep.
moose-gui = \{ {dep_args | unescaped} }
moose-clap = \{ {dep_args | unescaped}, optional = true }
moose-vst3 = \{ {dep_args | unescaped}, optional = true }
moose-au = \{ {dep_args | unescaped}, optional = true }
{{ if has_standalone -}}
moose-standalone = \{ {dep_args | unescaped}, features = ["gui"], optional = true }
{{ endif -}}
clap-sys = \{ version = "0.5", optional = true }
{{ if is_workspace }}{{ else }}
# Custom profile for `cargo moose install --shell`. The shell-mode
# build (`cargo build --profile shell --features ...,shell`) lands the
# shell binary at `target/shell/lib<crate>.dylib`, independent of
# `target/release/` (where regular `cargo build --release` writes) and
# `target/debug/` (where `cargo build` writes). Inherits release for
# DSP perf parity.
[profile.shell]
inherits = "release"
{{ endif }}