#!/usr/bin/env bash
# Release-version integrity: the git tag and the manifest version are the
# same fact, so they must never drift.
#
#   (no args)        single-source mode — the workspace root's
#                    [workspace.package].version is the ONLY `version =`
#                    declaration in the repo; every member inherits it via
#                    `version.workspace = true`. A release bump therefore
#                    touches exactly one line (gates leg "version", and the
#                    same check runs in CI's build workflow).
#   --tag <vX.Y.Z>   additionally require the tag name to equal the workspace
#                    version — the release-version workflow runs this on
#                    every v* tag push, so a tag whose name disagrees with
#                    the tagged commit's manifest cannot go green.
#
# Exit 0 only when every selected check passes.

set -euo pipefail

cd "$(dirname "$0")/.."

fail() {
    echo "check_release_version: FAIL: $1" >&2
    exit 1
}

root_version=$(awk '
    /^\[workspace\.package\]/ { inws = 1; next }
    /^\[/                     { inws = 0 }
    inws && /^version = /     { gsub(/"/, "", $3); print $3; exit }
' Cargo.toml)
[[ "$root_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || fail "workspace version '$root_version' is not X.Y.Z"

while IFS= read -r manifest; do
    grep -q '^version\.workspace = true' "$manifest" \
        || fail "$manifest declares its own version; inherit [workspace.package].version (version.workspace = true) instead"
done < <(find crates -name Cargo.toml | sort)

if [[ "${1:-}" == "--tag" ]]; then
    tag="${2:?usage: check_release_version.sh --tag vX.Y.Z}"
    [[ "$tag" == "v$root_version" ]] \
        || fail "tag '$tag' does not match the tagged commit's workspace version '$root_version'"
fi

echo "check_release_version: OK (workspace version $root_version)"
