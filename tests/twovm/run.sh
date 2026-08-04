#!/usr/bin/env bash
# Two-host end-to-end tests for blooter (design/TESTS.md).
#
# blooter and the host it talks to run on two separate machines -- separate
# kernels, separate bluetoothd, separate pairing agents, separate bond storage --
# joined by an emulated radio on this host. That is what makes bonding, and the
# ways the two halves of a bond stop agreeing, testable at all.
#
#   ./run.sh                     # whole suite
#   ./run.sh cold_pair           # only tests whose name contains "cold_pair"
#   ./run.sh d1 --repeat 5       # that selection, five times, to pin a flake
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

BLUEZ_VERSION="${BLUEZ_VERSION:-5.87}"
BUILD="$ROOT/tests/btvirt/build/bluez-$BLUEZ_VERSION"
export BTVIRT="$BUILD/emulator/btvirt"
export BTMGMT="$BUILD/tools/btmgmt"
export BTPROXY="$BUILD/tools/btproxy"
export BTMON="$BUILD/monitor/btmon"

if ! command -v vng >/dev/null 2>&1; then
    cat >&2 <<'EOF'
virtme-ng (vng) not found. Install it:

  sudo dnf install virtme-ng
  # or, without root:  pipx install virtme-ng

This suite boots *two* VMs at once, so budget memory for both (1G each by
default; set VNG_MEMORY to change it).
EOF
    exit 2
fi

echo "##### 1/3  building blooter #####"
cargo build --manifest-path "$ROOT/Cargo.toml"
export BLOOTER="$ROOT/target/debug/blooter"
[[ -x "$BLOOTER" ]] || { echo "blooter binary missing: $BLOOTER" >&2; exit 2; }

echo
echo "##### 2/3  building btvirt + btmgmt + btproxy + btmon #####"
# Shared with the btvirt suite: one bluez source tree, one set of binaries.
# btproxy is what bridges each guest's /dev/vhci onto the radio here.
"$ROOT/tests/btvirt/build-btvirt.sh"
for tool in "$BTVIRT" "$BTMGMT" "$BTPROXY"; do
    [[ -x "$tool" ]] || { echo "missing: $tool" >&2; exit 2; }
done
[[ -x "$BTMON" ]] || echo "note: btmon missing -- per-test HCI captures disabled"

echo
echo "##### 3/3  running tests (two VMs) #####"
# hub.py starts btvirt -t, boots both VMs, sequences their connection to the
# radio (order fixes addresses -- design/TESTS.md §2.3) and runs the suite.
exec python3 "$HERE/hub.py" "$@"
