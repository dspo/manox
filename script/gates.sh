#!/usr/bin/env bash
# The single gate authority for this repo (review round 3, P0-3).
#
# Every gate claim in a commit message or PR update must cite THIS script's
# output — hand-picked `cargo test` invocations proved unreliable (three
# false-green incidents inside PR 765: 9a508733, 2c544a30, f1a22f1f). The
# legs mirror CI plus the one thing CI cannot see on a developer machine:
#
#   1. fmt        — cargo fmt --all -- --check
#   2. clippy     — cargo clippy --workspace --all-targets -- -D warnings
#   3. test-real  — cargo test --workspace --all-targets --no-fail-fast
#                   under the developer's real HOME
#   4. test-clean — the same under a pristine temp HOME (the CI-equivalent
#                   hermeticity leg: no ~/.manox config, no provider
#                   registry, no runtime.lock contention with a live app)
#
# Usage: script/gates.sh [--quick]
#   --quick  fmt + clippy + test-real only (iteration; a commit gate still
#            requires the full run)
#
# Exit code: 0 only when every selected leg passes. Each leg prints a
# one-line PASS/FAIL verdict with its duration; the summary is the block to
# paste into commit messages.

set -u

QUICK=0
if [[ "${1:-}" == "--quick" ]]; then
    QUICK=1
fi

cd "$(dirname "$0")/.." || exit 1

declare -a NAMES=()
declare -a RESULTS=()
declare -a SECONDS_TAKEN=()

run_leg() {
    local name="$1"
    shift
    local start=$SECONDS
    echo ""
    echo "════════════════════════════════════════════"
    echo "GATE: $name"
    echo "════════════════════════════════════════════"
    if "$@"; then
        NAMES+=("$name"); RESULTS+=("PASS"); SECONDS_TAKEN+=("$((SECONDS - start))")
    else
        NAMES+=("$name"); RESULTS+=("FAIL"); SECONDS_TAKEN+=("$((SECONDS - start))")
    fi
}

run_leg "fmt" cargo fmt --all -- --check
# The production-unit shape: no dev-deps, no reverse-dependency feature
# enablement — cfg-gated items whose consumers are all gated are
# dead_code HERE, exactly the unit CI compiles (the a6df124e TEST_HOME
# lesson: the local --all-targets unification can mask it).
run_leg "prod-libs" cargo check \
    -p manox-agent -p manox-session-core -p manox-protocol \
    -p manox-harness -p manox-napi -p cx --lib
run_leg "clippy" cargo clippy --workspace --all-targets -- -D warnings
run_leg "test-real" cargo test --workspace --all-targets --no-fail-fast

if [[ "$QUICK" -eq 0 ]]; then
    # SHORT path on purpose: macOS unix-socket binds cap at 104 chars, and
    # `mktemp -d` under /var/folders/... pushes the ext-agents session
    # socket (`<home>/.manox/sessions/<32-hex>.sock`) over the limit — a
    # platform artifact, not a hermeticity signal. /tmp keeps every
    # derived path short (CI's /home/runner is short for the same reason).
    CLEAN_HOME="/tmp/manox-gates-home-$$"
    rm -rf "$CLEAN_HOME"
    mkdir -p "$CLEAN_HOME"
    # The clean leg must not inherit the developer's ~/.manox (provider
    # config, runtime.lock, session journals) — that is exactly the
    # hermeticity the CI runner enforces and the P0-1 regressions hid from.
    run_leg "test-clean" env HOME="$CLEAN_HOME" \
        cargo test --workspace --all-targets --no-fail-fast
    rm -rf "$CLEAN_HOME"
fi

echo ""
echo "════════════════════════════════════════════"
echo "GATES SUMMARY (script/gates.sh)"
echo "════════════════════════════════════════════"
EXIT=0
for i in "${!NAMES[@]}"; do
    printf '%-12s %s (%ss)\n' "${NAMES[$i]}" "${RESULTS[$i]}" "${SECONDS_TAKEN[$i]}"
    if [[ "${RESULTS[$i]}" != "PASS" ]]; then
        EXIT=1
    fi
done
exit "$EXIT"
