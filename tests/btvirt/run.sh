#!/usr/bin/env bash
# End-to-end connection tests for blooter.
#
# Builds blooter and btvirt on the host (unprivileged), then runs the suite
# inside a virtme-ng VM where we are root and can reach /dev/vhci and bind the
# low L2CAP PSMs. See README.md for why a VM is required.
#
#   ./run.sh                     # whole suite
#   ./run.sh keyboard            # only tests whose name contains "keyboard"
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
FILTER="${1:-}"

BLUEZ_VERSION="${BLUEZ_VERSION:-5.87}"
BTVIRT="$HERE/build/bluez-$BLUEZ_VERSION/emulator/btvirt"

if ! command -v vng >/dev/null 2>&1; then
    cat >&2 <<'EOF'
virtme-ng (vng) not found. Install it:

  sudo dnf install virtme-ng
  # or, without root:  pipx install virtme-ng

This is the only step needing root: /dev/kvm is world-writable and the host
kernel is world-readable, so the VM itself runs as your user.
EOF
    exit 2
fi

echo "##### 1/3  building blooter #####"
cargo build --manifest-path "$ROOT/Cargo.toml"
BLOOTER="$ROOT/target/debug/blooter"
[[ -x "$BLOOTER" ]] || { echo "blooter binary missing: $BLOOTER" >&2; exit 2; }

echo
echo "##### 2/3  building btvirt #####"
# btvirt is not packaged on Fedora, so we build it from bluez source into
# build/ here. Nothing is installed system-wide; the build is skipped once the
# binary exists.
"$HERE/build-btvirt.sh"
[[ -x "$BTVIRT" ]] || { echo "btvirt missing: $BTVIRT" >&2; exit 2; }

echo
echo "##### 3/3  running tests in VM #####"
# -r          boot the running host kernel
# --user root guest commands run as root, so /dev/vhci opens and the low PSMs bind
vng -r --user root \
    -e "BTVIRT='$BTVIRT' BLOOTER='$BLOOTER' FILTER='$FILTER' $HERE/guest/run-tests.sh"
