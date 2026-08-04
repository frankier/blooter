#!/usr/bin/env bash
# Guest-side entry point: runs as root inside one of the two virtme-ng VMs.
#
# Its only job is to get the agent talking to the hub on the host; everything
# after that is driven from there (see ../README.md).
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROLE="${ROLE:?ROLE not set}"

if [[ "$(id -u)" != 0 ]]; then
    echo "ERROR: must run as root inside the guest" >&2
    exit 2
fi

echo "role   : $ROLE"
echo "kernel : $(uname -r)"
echo "hub    : ${HUB_ADDR:-10.0.2.2}:${HUB_PORT:-45551}"
echo "radio  : ${RADIO_ADDR:-10.0.2.2}:${RADIO_PORT:-45550}"

# Slirp's guest->host route is what everything depends on, and a guest whose
# network never came up otherwise fails much later as an unexplained timeout
# waiting for an agent that is trying to connect and cannot.
for _ in $(seq 1 60); do
    ping -c1 -W1 "${HUB_ADDR:-10.0.2.2}" >/dev/null 2>&1 && break
    sleep 0.5
done

# A writable run directory for logs, the FIFO and blooter's state; the host
# filesystem is mounted read-only under virtme-ng, so nothing here touches it.
mkdir -p "/tmp/blooter-twovm-$ROLE"

exec python3 "$HERE/agent.py" "$ROLE"
