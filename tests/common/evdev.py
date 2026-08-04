"""`struct input_event` synthesis and parsing, plus the codes the suites use.

blooter's FIFO mode (`-f`) reads native `input_event` records straight off a
pipe, which is what makes it testable without evdev; `tests/twovm` also reads
real records back out of `/dev/input/eventN` on the host VM, so the same struct
serves both directions.
"""

import struct

# Native `struct input_event`: timeval(16) + type(2) + code(2) + value(4) = 24
# bytes on 64-bit, which is what input.rs::spawn_fifo parses.
INPUT_EVENT = struct.Struct("@llHHi")
assert INPUT_EVENT.size == 24, f"unexpected input_event size {INPUT_EVENT.size}"

# evdev event types/codes (linux/input-event-codes.h).
EV_SYN = 0x00
EV_KEY = 0x01
EV_REL = 0x02
EV_MSC = 0x04
# SYN_REPORT: end of an input frame. blooter only emits a report at a frame
# boundary (design/ARCH.md §7.2c), so every injected event needs one after it.
SYN_REPORT = 0x00

KEY_A = 30
KEY_B = 48
KEY_T = 20
KEY_LEFTMETA = 125
KEY_VOLUMEUP = 115
# blooter's default capture-toggle chord (cli.rs USAGE).
KEY_LEFTSHIFT = 42
KEY_RIGHTSHIFT = 54

BTN_LEFT = 0x110
BTN_RIGHT = 0x111

REL_X = 0x00
REL_Y = 0x01

# Consumer-page usages (design/REMOTE.md §2).
CONSUMER_VOLUME_UP = 0x0E9
CONSUMER_TV = 0x089


def input_event(type_, code, value):
    return INPUT_EVENT.pack(0, 0, type_, code, value)


def frame(*events):
    """One input frame: the given events followed by the SYN_REPORT that makes
    blooter emit a report."""
    return b"".join(events) + input_event(EV_SYN, SYN_REPORT, 0)


def parse_input_events(data):
    """Decode a buffer read from an evdev node into (type, code, value)."""
    out = []
    for offset in range(0, len(data) - INPUT_EVENT.size + 1, INPUT_EVENT.size):
        _sec, _usec, type_, code, value = INPUT_EVENT.unpack_from(data, offset)
        out.append((type_, code, value))
    return out
