"""Pieces shared by more than one blooter integration suite.

`tests/btvirt` and `tests/twovm` both bring up a bluetoothd on a private system
bus, both drive blooter's FIFO input, and both need a test runner (the guests
have no pytest). Those parts live here rather than being copied, because they
are the parts most likely to drift apart silently -- the two suites want
*identical* daemon startup, or a failure in one stops meaning anything about the
other.

What stays suite-local is what genuinely differs: how the controllers come into
existence (btvirt's `-l2` vs twovm's `btproxy`), and what plays the host.
"""

from .evdev import (
    BTN_LEFT,
    BTN_RIGHT,
    EV_KEY,
    EV_REL,
    EV_SYN,
    INPUT_EVENT,
    KEY_A,
    KEY_B,
    KEY_LEFTMETA,
    KEY_LEFTSHIFT,
    KEY_RIGHTSHIFT,
    KEY_T,
    KEY_VOLUMEUP,
    REL_X,
    REL_Y,
    SYN_REPORT,
    frame,
    input_event,
    parse_input_events,
)
from .process import (
    ANSI,
    HarnessError,
    Process,
    PtyProcess,
    log,
    rundir,
    set_rundir,
    wait_for,
)
from .runner import Registry
from .stack import BluezStack, btmgmt_path, run_btmgmt

__all__ = [
    "ANSI",
    "BTN_LEFT",
    "BTN_RIGHT",
    "BluezStack",
    "EV_KEY",
    "EV_REL",
    "EV_SYN",
    "HarnessError",
    "INPUT_EVENT",
    "KEY_A",
    "KEY_B",
    "KEY_LEFTMETA",
    "KEY_LEFTSHIFT",
    "KEY_RIGHTSHIFT",
    "KEY_T",
    "KEY_VOLUMEUP",
    "Process",
    "PtyProcess",
    "REL_X",
    "REL_Y",
    "Registry",
    "SYN_REPORT",
    "btmgmt_path",
    "frame",
    "input_event",
    "log",
    "parse_input_events",
    "run_btmgmt",
    "rundir",
    "set_rundir",
    "wait_for",
]
