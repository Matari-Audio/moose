#!/usr/bin/env bash
# Thin wrapper over recursive-cargo.sh that runs `cargo moose <args>` in
# every workspace listed by moose-workspaces.sh. See
# recursive-cargo.sh for the workspace list, color handling, and
# [OK]/[SKIP]/[FAIL] semantics. moose reports a plugin that lives in a
# different workspace as "No plugin with crate name", which we add to the
# skip-pattern alternation so that miss is treated as [SKIP], not [FAIL].
#
# Usage: recursive-cargo-moose.sh <cargo-moose-args>
# Examples:
#   recursive-cargo-moose.sh build -p moose-example-gain --clap
#   recursive-cargo-moose.sh install -p moose-example-gain --clap
#   recursive-cargo-moose.sh package --vst3
set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ $# -eq 0 ]]; then
    cat >&2 <<EOF
usage: $(basename "$0") <cargo-moose-args>

Runs 'cargo moose <args>' in every workspace listed by moose-workspaces.sh.
EOF
    exit 64
fi

export RECURSIVE_CARGO_SKIP_PATTERN='no plugin with crate name'
exec "$script_dir/recursive-cargo.sh" moose "$@"
