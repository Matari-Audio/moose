#!/usr/bin/env bash
#
# conformance.sh - build example plugins and run the format validators on
# every bundle: clap-validator on .clap, the VST3 SDK validator and
# pluginval on .vst3. Runs headless; pluginval skips its GUI tests.
#
#   scripts/ci/conformance.sh [example-package ...]
#
# With no arguments it checks one plugin of each shape: an effect, an
# instrument, a sidechain effect and a multi-port note plugin. Validators
# come from PATH or from CLAP_VALIDATOR, VST3_VALIDATOR and PLUGINVAL.
# PLUGINVAL_STRICTNESS defaults to 5. Every validator runs on every bundle
# before the script fails, so one run shows every failure.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

CARGO_MOOSE=${CARGO_MOOSE:-cargo-moose}
CLAP_VALIDATOR=${CLAP_VALIDATOR:-clap-validator}
VST3_VALIDATOR=${VST3_VALIDATOR:-validator}
PLUGINVAL=${PLUGINVAL:-pluginval}
STRICTNESS=${PLUGINVAL_STRICTNESS:-5}

if [[ $# -gt 0 ]]; then
    examples=("$@")
else
    examples=(moose-example-gain moose-example-synth moose-example-sidechain moose-example-multiport)
fi

for tool in "$CARGO_MOOSE" "$CLAP_VALIDATOR" "$VST3_VALIDATOR" "$PLUGINVAL"; do
    command -v "$tool" >/dev/null || { echo "::error::$tool not found" >&2; exit 1; }
done

clap=()
vst3=()
for example in "${examples[@]}"; do
    log=$("$CARGO_MOOSE" moose build -p "$example" --clap --vst3 2>&1) || {
        echo "$log" >&2
        echo "::error::build failed: $example" >&2
        exit 1
    }
    while IFS= read -r path; do clap+=("$path"); done < <(sed -nE 's/^ +CLAP: (.*)$/\1/p' <<<"$log")
    while IFS= read -r path; do vst3+=("$path"); done < <(sed -nE 's/^ +VST3: (.*)$/\1/p' <<<"$log")
done

failed=()
check() {
    local name=$1; shift
    echo "::group::$name"
    if "$@"; then
        echo "::endgroup::"
    else
        echo "::endgroup::"
        echo "::error::$name failed"
        failed+=("$name")
    fi
}

for bundle in "${clap[@]}"; do
    check "clap-validator $(basename "$bundle")" "$CLAP_VALIDATOR" validate "$bundle"
done
for bundle in "${vst3[@]}"; do
    check "vst3 validator $(basename "$bundle")" "$VST3_VALIDATOR" "$bundle"
    check "pluginval $(basename "$bundle")" "$PLUGINVAL" --strictness-level "$STRICTNESS" \
        --validate-in-process --skip-gui-tests --validate "$bundle"
done

echo "checked ${#clap[@]} CLAP and ${#vst3[@]} VST3 bundles"
if [[ ${#failed[@]} -gt 0 ]]; then
    printf 'FAILED: %s\n' "${failed[@]}" >&2
    exit 1
fi
