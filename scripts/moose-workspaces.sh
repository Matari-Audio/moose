#!/usr/bin/env bash
# Single source of truth for the workspace tree. Currently just the main
# workspace; add a line here if a crate ever needs its own Cargo
# workspace again.
#
# Sourced by recursive-cargo.sh and supply-chain.sh. Not executable on
# its own. `moose_workspaces <repo-root>` prints each workspace's
# absolute path, one per line, main first.
moose_workspaces() {
    local root="$1"
    printf '%s\n' \
        "$root"
}
