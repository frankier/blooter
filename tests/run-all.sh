#!/usr/bin/env bash
# Run every blooter test suite.
#
#   ./run-all.sh            # unit tests, then both integration suites
#   ./run-all.sh btvirt     # just one suite (btvirt | termdbus | unit)
#
# Each suite keeps running even if an earlier one fails, so one invocation
# reports the whole picture rather than stopping at the first problem.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
ONLY="${1:-}"

declare -a NAMES=() RESULTS=()
overall=0

run_suite() {
    local name="$1"; shift
    if [[ -n "$ONLY" && "$ONLY" != "$name" ]]; then
        return
    fi
    echo
    echo "################################################################"
    echo "#  $name"
    echo "################################################################"
    if "$@"; then
        NAMES+=("$name"); RESULTS+=("pass")
    else
        local rc=$?
        NAMES+=("$name"); RESULTS+=("FAIL (rc=$rc)")
        overall=1
    fi
}

# Unit tests first: they are fast and need no VM, so a broken build or a broken
# report/keymap shows up before spending time booting.
run_suite unit cargo test --manifest-path "$ROOT/Cargo.toml"

# Real L2CAP link against emulated controllers (non-interactive).
run_suite btvirt "$HERE/btvirt/run.sh"

# Interactive menu and pairing prompt, on a PTY against a mocked BlueZ.
run_suite termdbus "$HERE/termdbus/run.sh"

echo
echo "################################################################"
echo "#  summary"
echo "################################################################"
for i in "${!NAMES[@]}"; do
    printf '  %-10s %s\n' "${NAMES[$i]}" "${RESULTS[$i]}"
done
[[ ${#NAMES[@]} -eq 0 ]] && { echo "  no suite matched '$ONLY'"; exit 2; }
exit $overall
