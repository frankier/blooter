#!/usr/bin/env bash
# Guest-side entry point: runs as root inside the virtme-ng VM.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export BLOOTER="${BLOOTER:?BLOOTER not set}"
FILTER="${FILTER:-}"

if [[ "$(id -u)" != 0 ]]; then
    echo "ERROR: must run as root inside the guest" >&2
    exit 2
fi

echo "kernel : $(uname -r)"
echo "blooter: $BLOOTER"

# blooter binds L2CAP PSMs 0x11/0x13 (below 0x1001, so CAP_NET_BIND_SERVICE is
# needed) even though these tests never bring a link up. No controller is
# required for the bind, hence no btvirt and no bluetoothd here.
mkdir -p /tmp/blooter-termdbus

exec python3 "$HERE/test_menu.py" "$FILTER"
