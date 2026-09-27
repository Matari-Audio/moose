#!/usr/bin/env python3
"""Rewrite scaffolded `moose-*` Cargo.toml dependencies to point at
local `path = "<workspace>/crates/<name>"` checkouts, so a scaffolded
project consumes moose HEAD from the checked-out workspace instead
of the registry version (or git tag) the scaffold template
generated against.

Handles both scaffold dep forms:

* **Registry pin** (current default, post-crates.io migration) ::

      moose-* = { version = "X.Y"[, ...] }
                            ↓
      moose-* = { path = "<crates>/<name>"[, ...] }

* **Git+tag pin** (pre-crates.io, opt-in via `cargo moose new
  --github`, and what last-released cargo-moose still emits) ::

      moose-* = { git = "https://github.com/Matari-Audio/moose",
                  tag = "vX.Y.Z"[, ...] }
                            ↓
      moose-* = { path = "<crates>/<name>"[, ...] }

Used by `.github/workflows/cli-e2e.yml` (scaffold + build with
current cargo-moose — exercises the registry path) and
`.github/workflows/cli-backcompat.yml` (scaffold with the
LAST released cargo-moose, then build against moose HEAD —
exercises the git+tag path while the last release still emits it).

Required env vars:
  GITHUB_WORKSPACE - the moose checkout (provides crates/* paths)
  SCAFFOLD_DIR     - absolute path to the scaffolded project root

Exits non-zero if no Cargo.toml in the scaffold tree contained a
rewritable moose-* dep - that's a signal the scaffold template
layout drifted and this script's patterns need updating to match.
"""

import os
import pathlib
import re
import sys

# Matches the leading half of a git+tag scaffold dep line:
#   moose-foo = { git = "https://github.com/Matari-Audio/moose", ...
GIT_NEEDLE = '{ git = "https://github.com/Matari-Audio/moose"'

# Strips `, tag = "..."` off a converted line — invalid on path
# deps, which is why we substitute it out instead of leaving it in.
GIT_TAG_RE = re.compile(r',\s*tag\s*=\s*"[^"]*"')

# Matches the leading half of a registry scaffold dep line, with
# the inline-table opener captured up to the closing `"` of the
# version literal:
#   moose-foo = { version = "0.48"
#   moose-foo = {version="0.48"
# The literal `{` plus optional whitespace plus `version = "..."`
# lets us splice in `{ path = "..."` without disturbing whatever
# trailing keys (`features`, `optional`, ...) come after.
REGISTRY_RE = re.compile(r'\{\s*version\s*=\s*"[^"]*"')


def rewrite_line(line: str, moose_crates: str) -> tuple[str, bool]:
    """Rewrite a single Cargo.toml line in place. Returns the new
    text and a flag indicating whether anything changed.

    Only lines whose left-hand key starts with `moose` are touched.
    Commented lines pass through. Non-moose deps (clap-sys, the
    `[package].version` field, etc.) pass through.
    """
    stripped = line.lstrip()
    if stripped.startswith("#"):
        return line, False

    eq = line.find("=")
    if eq < 0:
        return line, False
    key = line[:eq].strip()
    if not key.startswith("moose"):
        return line, False

    replacement = f'{{ path = "{moose_crates}/{key}"'

    if GIT_NEEDLE in line:
        out = line.replace(GIT_NEEDLE, replacement, 1)
        out = GIT_TAG_RE.sub("", out, count=1)
        return out, True

    if REGISTRY_RE.search(line):
        out = REGISTRY_RE.sub(replacement, line, count=1)
        return out, True

    return line, False


def main() -> int:
    moose_crates = pathlib.Path(os.environ["GITHUB_WORKSPACE"], "crates").as_posix()
    root = pathlib.Path(os.environ["SCAFFOLD_DIR"])

    rewrote = 0
    for path in root.rglob("Cargo.toml"):
        content = path.read_text()
        new_lines = []
        touched = False
        for line in content.splitlines():
            new_line, changed = rewrite_line(line, moose_crates)
            if changed:
                touched = True
            new_lines.append(new_line)
        if not touched:
            continue
        suffix = "\n" if content.endswith("\n") else ""
        path.write_text("\n".join(new_lines) + suffix)
        rewrote += 1
        print(f"rewrote {path}")

    if rewrote == 0:
        print(
            "::error::no Cargo.toml had a rewritable moose-* dep "
            "(neither git+tag nor registry form) - scaffold layout "
            "may have changed",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
