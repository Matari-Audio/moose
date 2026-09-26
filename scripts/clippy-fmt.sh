#!/usr/bin/env bash
# Run `cargo clippy --fix --allow-dirty --all-features --all-targets`
# followed by `cargo fmt` in every workspace listed by
# truce-workspaces.sh. The verification gate before declaring a change
# done.
#
# Sequential (not parallel) so stdout / stderr interleave cleanly.

set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root_dir="$(cd "$script_dir/.." && pwd)"

# Under WSL the workspace lives on a Windows drive, where the native
# `cargo.exe` toolchain builds far faster than the Linux one (and
# matches what the user actually ships). Prefer it when present, but
# fall back to plain `cargo` on real Linux / macOS where it's absent.
cargo="cargo"
if command -v cargo.exe >/dev/null 2>&1; then
    cargo="cargo.exe"
fi

# shellcheck source=truce-workspaces.sh
source "$script_dir/truce-workspaces.sh"

# `mapfile` is bash 4+; build by hand for macOS's stock bash 3.2.
workspaces=()
while IFS= read -r ws_path; do
    workspaces+=("$ws_path")
done < <(truce_workspaces "$root_dir")

overall_status=0
for ws in "${workspaces[@]}"; do
    label="${ws#"$root_dir"}"
    label="${label#/}"
    [[ -z "$label" ]] && label="(main)"
    printf '\n=== clippy --fix [%s] ===\n' "$label"
    if ! ( cd "$ws" && "$cargo" clippy --fix --allow-dirty \
        --all-features --all-targets ); then
        rc=$?
        printf '[FAIL] clippy %s (exit %d)\n' "$label" "$rc" >&2
        overall_status=$rc
        continue
    fi
    printf '\n=== fmt [%s] ===\n' "$label"
    if ! ( cd "$ws" && "$cargo" fmt ); then
        rc=$?
        printf '[FAIL] fmt %s (exit %d)\n' "$label" "$rc" >&2
        overall_status=$rc
        continue
    fi
    printf '[ OK ] %s\n' "$label"
done

exit "$overall_status"
