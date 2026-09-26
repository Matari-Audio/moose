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
truce = \{ version = "{version}" }
truce-gui = \{ version = "{version}" }
truce-gui-types = \{ version = "{version}" }
truce-clap = \{ version = "{version}" }
truce-vst3 = \{ version = "{version}" }
{{ if has_standalone -}}
truce-standalone = \{ version = "{version}" }
{{ endif -}}
{{- else -}}
truce = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
truce-gui = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
truce-gui-types = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
truce-clap = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
truce-vst3 = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
{{ if has_standalone -}}
truce-standalone = \{ git = "https://github.com/truce-audio/truce", tag = "{tag}" }
{{ endif -}}
{{- endif }}
clap-sys = "0.5"

# Custom profile for `cargo truce install --shell`. The shell-mode
# build lands at `target/shell/lib<crate>.dylib`, independent of
# `target/release/` and `target/debug/`. Cargo profiles are workspace-
# level so this entry covers every plugin in the workspace.
[profile.shell]
inherits = "release"
