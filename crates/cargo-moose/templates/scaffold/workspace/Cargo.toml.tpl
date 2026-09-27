[workspace]
resolver = "3"
members = [
{{- for m in members }}
    "{m}",
{{- endfor }}
]

[workspace.package]
version = "0.1.0"
edition = "2024"

[workspace.dependencies]
{{ if use_registry -}}
moose = \{ version = "{version}" }
moose-gui = \{ version = "{version}" }
moose-gui-types = \{ version = "{version}" }
moose-clap = \{ version = "{version}" }
moose-vst3 = \{ version = "{version}" }
moose-au = \{ version = "{version}" }
{{ if has_standalone -}}
moose-standalone = \{ version = "{version}" }
{{ endif -}}
{{- else -}}
moose = \{ git = "https://github.com/Matari-Audio/moose", tag = "{tag}" }
moose-gui = \{ git = "https://github.com/Matari-Audio/moose", tag = "{tag}" }
moose-gui-types = \{ git = "https://github.com/Matari-Audio/moose", tag = "{tag}" }
moose-clap = \{ git = "https://github.com/Matari-Audio/moose", tag = "{tag}" }
moose-vst3 = \{ git = "https://github.com/Matari-Audio/moose", tag = "{tag}" }
moose-au = \{ git = "https://github.com/Matari-Audio/moose", tag = "{tag}" }
{{ if has_standalone -}}
moose-standalone = \{ git = "https://github.com/Matari-Audio/moose", tag = "{tag}" }
{{ endif -}}
{{- endif }}
clap-sys = "0.5"

# Custom profile for `cargo moose install --shell`. The shell-mode
# build lands at `target/shell/lib<crate>.dylib`, independent of
# `target/release/` and `target/debug/`. Cargo profiles are workspace-
# level so this entry covers every plugin in the workspace.
[profile.shell]
inherits = "release"
