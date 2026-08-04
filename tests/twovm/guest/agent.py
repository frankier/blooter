#!/usr/bin/env python3
"""In-VM agent: one per guest, dialing the hub on the host.

Slirp puts each guest on its own isolated network, so the two VMs cannot reach
each other -- only the host (design/TESTS.md §2.1). Every guest therefore talks
to `hub.py` and nothing else, over one TCP connection carrying newline-delimited
JSON:

    hub   -> agent   {"id": 7, "cmd": "start_blooter", "args": {...}}
    agent -> hub     {"id": 7, "ok": true, "result": {...}}

The agent is deliberately thin: start and stop daemons, run `bluetoothctl`, open
evdev nodes, write the FIFO. **No test logic lives here** -- a new scenario is a
host-side function and nothing else, which is what keeps the tests readable as
one linear story despite spanning two machines.

`dev.py` and `host.py` supply the role-specific command tables.
"""

import json
import os
import socket
import sys
import traceback

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(
    os.path.abspath(__file__)))))

from common import HarnessError, log, set_rundir  # noqa: E402


def serve(role, hub_addr, hub_port):
    rundir = set_rundir(f"/tmp/blooter-twovm-{role}")
    log(f"role {role}, run directory {rundir}")

    if role == "dev":
        import dev
        agent = dev.DevAgent()
    elif role == "host":
        import host
        agent = host.HostAgent()
    else:
        raise HarnessError(f"unknown role {role!r}")

    sock = socket.create_connection((hub_addr, hub_port), timeout=60.0)
    # Commands can legitimately take a long time (a bond negotiation, a
    # bluetoothd restart), and the hub applies its own per-call deadline, so the
    # agent must not impose a second, shorter one.
    sock.settimeout(None)
    stream = sock.makefile("rwb")
    _send(stream, {"hello": role, "commands": sorted(agent.commands)})
    log(f"registered with the hub at {hub_addr}:{hub_port}")

    try:
        for line in stream:
            line = line.strip()
            if not line:
                continue
            request = json.loads(line)
            _send(stream, _dispatch(agent, request))
    finally:
        try:
            agent.shutdown()
        except Exception:  # noqa: BLE001 - teardown must not mask the exit
            traceback.print_exc()


def _dispatch(agent, request):
    ident, name, args = request.get("id"), request.get("cmd"), request.get("args", {})
    handler = agent.commands.get(name)
    if handler is None:
        return {"id": ident, "ok": False,
                "error": f"{agent.role}: no such command {name!r}"}
    try:
        return {"id": ident, "ok": True, "result": handler(**args)}
    except Exception as exc:  # noqa: BLE001 - reported across the wire
        # The traceback travels too: a guest-side failure is otherwise a bare
        # string on the host, three machines away from where it happened.
        return {"id": ident, "ok": False,
                "error": f"{type(exc).__name__}: {exc}",
                "trace": traceback.format_exc()}


def _send(stream, obj):
    stream.write(json.dumps(obj).encode() + b"\n")
    stream.flush()


def main():
    role = os.environ.get("ROLE") or (sys.argv[1] if len(sys.argv) > 1 else "")
    hub_addr = os.environ.get("HUB_ADDR", "10.0.2.2")
    hub_port = int(os.environ.get("HUB_PORT", "45551"))
    if os.geteuid() != 0:
        print("ERROR: the agent must run as root inside the guest",
              file=sys.stderr)
        return 2
    try:
        serve(role, hub_addr, hub_port)
    except Exception:  # noqa: BLE001 - the guest console is the only reporter
        traceback.print_exc()
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
