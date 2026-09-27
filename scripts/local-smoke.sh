#!/usr/bin/env bash
# Local smoke test: like post-publish-smoke.sh but against a checked-out
# moose. Installs cargo-moose from the checkout and patches every local
# moose crate into the scaffolded project, so nothing comes from crates.io.
# Run under bash. Usage: local-smoke.sh [moose-dir]
#   env: RUN_SECS=N (CI auto-close), KEEP=1
set -euo pipefail

RUN_SECS="${RUN_SECS:-}"
ok()  { printf '[ OK ] %s\n' "$*"; }
die() { printf '[FAIL] %s\n' "$*" >&2; exit 1; }

SELF="$(cd "$(dirname "$0")" && pwd)"
MOOSE_DIR="$(cd "${1:-$SELF/..}" && pwd)"
[ -d "$MOOSE_DIR/crates/cargo-moose" ] || die "not a moose checkout: $MOOSE_DIR"

case "$(uname -s)" in
  Darwin)               OS=macos;   EXE=""     ;;
  Linux)                OS=linux;   EXE=""     ;;
  MINGW*|MSYS*|CYGWIN*) OS=windows; EXE=".exe" ;;
  *) die "unsupported OS: $(uname -s)" ;;
esac
command -v cargo >/dev/null || die "cargo not on PATH"

WORK="$(mktemp -d /tmp/moose-local-smoke.XXXXXX)"
APP_PID=""
cleanup() {
  [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
  [ "${KEEP:-0}" = 1 ] && { echo "kept: $WORK"; return; }
  rm -rf "$WORK"
}
trap cleanup EXIT

# 1. install cargo-moose from the local checkout
cargo install --path "$MOOSE_DIR/crates/cargo-moose" --force --locked
cargo moose --help >/dev/null || die "'cargo moose' not runnable"
ok "installed local cargo-moose"

# 2. scaffold defaults
( cd "$WORK" && cargo moose new smoketest )
PROJ="$WORK/smoketest"
[ -f "$PROJ/Cargo.toml" ] || die "scaffold produced no Cargo.toml"

# 3. patch every local moose-* crate (+ optional baseview) into the project.
#    Unused-patch warnings for non-dep crates are expected.
{
  echo ""
  echo "[patch.crates-io]"
  for toml in "$MOOSE_DIR"/crates/*/Cargo.toml; do
    name="$(grep -m1 '^name *= *"' "$toml" | cut -d'"' -f2)"
    case "$name" in
      cargo-moose|"") continue ;;   # CLI, not a dep
      moose*) printf '%s = { path = "%s" }\n' "$name" "$(dirname "$toml")" ;;
    esac
  done
} >> "$PROJ/Cargo.toml"
ok "patched moose deps -> $MOOSE_DIR"

# 4. build standalone against local crates
( cd "$PROJ" && cargo build --release )
BIN="$PROJ/target/release/smoketest-standalone$EXE"
[ -x "$BIN" ] || die "no standalone binary at $BIN"
ok "built standalone"

# 5. launch: foreground by default (close manually); RUN_SECS=N auto-closes (CI)
cmd=()
if [ "$OS" = linux ] && [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
  command -v xvfb-run >/dev/null && cmd+=(xvfb-run -a) || echo "warn: no DISPLAY / xvfb-run"
fi
cmd+=("$BIN")
if [ -z "$RUN_SECS" ]; then
  echo "launching standalone — close the window to finish"
  rc=0; "${cmd[@]}" || rc=$?
  [ "$rc" = 0 ] || die "standalone exited $rc"
else
  LOG="$WORK/standalone.log"
  "${cmd[@]}" >"$LOG" 2>&1 &
  APP_PID=$!
  sleep "$RUN_SECS"
  if kill -0 "$APP_PID" 2>/dev/null; then
    kill "$APP_PID" 2>/dev/null || true; wait "$APP_PID" 2>/dev/null || true
    ok "standalone ran ${RUN_SECS}s, no crash"
  else
    rc=0; wait "$APP_PID" || rc=$?
    [ "$rc" = 0 ] || { cat "$LOG" || true; die "standalone exited $rc"; }
  fi
  APP_PID=""
fi
ok "smoke test passed"
