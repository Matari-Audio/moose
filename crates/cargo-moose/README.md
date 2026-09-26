# cargo-moose

Cargo subcommand for building moose audio plugins.

## Overview

The `cargo moose` CLI. Handles scaffolding new plugin projects,
building and bundling for all supported formats, code signing,
installing into the host's plug-in directories, and validating with
host-specific tools.

## Installation

```sh
cargo install cargo-moose
```

## Commands

```sh
cargo moose new my-plugin          # scaffold a single-plugin project (clap+vst3+standalone by default)
cargo moose new my-plugin --no-standalone  # ... without the standalone host bin
cargo moose new my-ws --workspace gain reverb  # scaffold a multi-plugin workspace
cargo moose install                # build + bundle + sign + install (per-user by default)
cargo moose install --system       # install for all users (sudo / admin)
cargo moose install --clap         # single format only
cargo moose build                  # bundle into target/bundles/ without installing
cargo moose package                # build a signed .pkg / .exe in target/dist/
cargo moose uninstall              # remove installed plugins (mirrors install scope flags)
cargo moose validate               # run pluginval (VST3) + clap-validator
cargo moose doctor                 # check toolchain, SDKs, signing certs, install paths
cargo moose run                    # build and launch standalone
cargo moose screenshot             # render every plugin's GUI to target/screenshots/
cargo moose status                 # show installed plugin versions
```

## Supported formats

CLAP, VST3, and standalone on macOS, Windows, and Linux.

## Library API

cargo-moose ships as both a binary and a library (`cargo_moose` crate).
The library half (`cargo_moose::run`, `cargo_moose::scaffold::*`) is the
engine for the build/install/package pipelines; the binary is a thin
arg-parsing shell that drives it. Embedding the engine in your own
tooling is supported but mostly intended for internal use - most
plugin authors only need the `cargo moose` CLI.

Part of [moose](https://github.com/Matari-Audio/moose). [Docs](https://truce.audio/docs/).
