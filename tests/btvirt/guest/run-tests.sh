#!/usr/bin/env bash
# Guest-side entry point: runs as root inside the virtme-ng VM.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BTVIRT="${BTVIRT:?BTVIRT not set}"
export BTMGMT="${BTMGMT:?BTMGMT not set}"
BLOOTER="${BLOOTER:?BLOOTER not set}"
FILTER="${FILTER:-}"

if [[ "$(id -u)" != 0 ]]; then
    echo "ERROR: must run as root inside the guest" >&2
    exit 2
fi

# blooter binds L2CAP PSMs 0x11/0x13, which are below 0x1001 and so need
# CAP_NET_BIND_SERVICE; as guest root we have it.
echo "kernel : $(uname -r)"
echo "btvirt : $BTVIRT"
echo "btmgmt : $BTMGMT"
echo "blooter: $BLOOTER"

# A writable /tmp for the run directory, FIFO and component logs. The host
# filesystem is mounted read-only under virtme-ng, so nothing here touches it.
mkdir -p /tmp/blooter-btvirt

exec python3 "$HERE/test_connection.py" "$BTVIRT" "$BLOOTER" $FILTER
