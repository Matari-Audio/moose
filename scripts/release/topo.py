#!/usr/bin/env python3
#
# topo.py — emit moose workspace crates in publish order, one per line.
#
# Filter: workspace members under `crates/` (examples are scaffolded
# demonstrations, not published, and crates that opt out via
# `publish = false` in their Cargo.toml are skipped). Topo-sort by
# intra-workspace dependencies so every dep is on the registry before
# its dependents.
#
# Output is TSV `<crate>\t<workspace_dir>` where `<workspace_dir>` is
# `.` (the main workspace). release.sh `cd`s into that dir before
# `cargo publish -p <crate>` so the right Cargo.lock and `[workspace]`
# context are picked up.
#
# Invoked by release.sh; runnable standalone for debugging the order.

import json
import os
import shutil
import subprocess
import sys


def cargo_bin():
    # Honor an explicit CARGO from the caller (release.sh). Otherwise
    # prefer cargo.exe — on Windows (WSL) both cargo.exe and cargo can
    # be on PATH — and fall back to cargo.
    explicit = os.environ.get("CARGO")
    if explicit:
        return explicit
    for cand in ("cargo.exe", "cargo"):
        if shutil.which(cand):
            return cand
    return "cargo"


CARGO = cargo_bin()


def topo_sort(pkgs):
    """Kahn topo-sort, alphabetical tie-break for determinism."""
    names = {p["name"] for p in pkgs}
    incoming = {p["name"]: set() for p in pkgs}
    for p in pkgs:
        for d in p["dependencies"]:
            if d.get("kind") == "dev":
                continue
            if d["name"] in names and d["name"] != p["name"]:
                incoming[p["name"]].add(d["name"])

    order = []
    ready = sorted(n for n, deps in incoming.items() if not deps)
    while ready:
        n = ready.pop(0)
        order.append(n)
        for m, deps in list(incoming.items()):
            if n in deps:
                deps.discard(n)
                if not deps and m not in order and m not in ready:
                    ready.append(m)
        ready.sort()

    remaining = [n for n in incoming if n not in order]
    if remaining:
        sys.exit(f"cycle: unresolved={remaining}")
    return order


def workspace_members(manifest_path=None, path_filter=None):
    """Return (pkgs, names) for a workspace's publishable members. If
    `manifest_path` is None, runs against cwd (main workspace).
    `path_filter` (when given) restricts to packages whose
    `manifest_path` contains the substring — used to drop test crates
    that live outside `crates/` in the main workspace."""
    args = [CARGO, "metadata", "--format-version", "1", "--no-deps"]
    if manifest_path:
        args += ["--manifest-path", manifest_path]
    meta = json.loads(subprocess.check_output(args))
    ws = set(meta["workspace_members"])
    pkgs = []
    for p in meta["packages"]:
        if p["id"] not in ws:
            continue
        if path_filter and path_filter not in p["manifest_path"]:
            continue
        if p.get("publish") == []:  # opt-out via `publish = false`
            continue
        pkgs.append(p)
    return pkgs


# ---------------------------------------------------------------------------
# Main workspace
# ---------------------------------------------------------------------------

main_pkgs = workspace_members(path_filter="/crates/")
main_order = topo_sort(main_pkgs)

# Force the ordering of user-selectable crates so the most consumer-
# facing surfaces (cargo-moose, moose, format wrappers) come last and
# fail loudly if a transitive dep didn't make it onto the registry.
forced_order = [
    "moose-simd",
    "moose-standalone",
    "moose-clap",
    "moose-vst3",
    "moose",
    "cargo-moose",
]

missing_forced = [name for name in forced_order if name not in main_order]
if missing_forced:
    sys.exit(
        "forced ordering crates missing from publish order: "
        + ", ".join(missing_forced)
    )

main_order = [n for n in main_order if n not in forced_order] + forced_order

# ---------------------------------------------------------------------------
# Emit
# ---------------------------------------------------------------------------

for name in main_order:
    print(f"{name}\t.")
