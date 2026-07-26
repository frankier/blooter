#!/usr/bin/env python3
"""End-to-end tests for blooter's connection logic.

Every test drives the real binary over real L2CAP from a second emulated
controller. See harness.py for the stack and design/CONNECTION.md for the
behaviour being pinned down.
"""

import sys

from harness import (
    BTN_LEFT,
    KEY_A,
    KEY_B,
    REL_X,
    REL_Y,
    Registry,
    assert_report,
    keyboard_report,
    mouse_report,
)

tests = Registry()


# --------------------------------------------------------------------------
# Link establishment (design/CONNECTION.md §3.1)
# --------------------------------------------------------------------------

@tests.test
def test_accepts_inbound_connection(t):
    """The acceptor path: host dials control then interrupt, blooter connects."""
    t.start_blooter()
    t.connected_host()


@tests.test
def test_control_only_does_not_connect(t):
    """Control alone must not count as a session: blooter requires the
    interrupt channel within 3s and otherwise goes back to accepting (§3.1)."""
    blooter = t.start_blooter()
    host = t.host()

    # Dial only the control PSM, then let the 3s interrupt window lapse.
    host.connect_control_only()
    try:
        blooter.wait_for_output(
            r"timed out waiting for interrupt channel", 12.0,
            "blooter to give up on the missing interrupt channel")
        # ...and it must still be accepting, not wedged or exited.
        assert blooter.proc.alive(), "blooter exited after an incomplete connect"
    finally:
        host.close()

    # A complete connection still works afterwards.
    t.connected_host()


@tests.test
def test_reconnect_after_disconnect(t):
    """A dropped link returns blooter to accepting, and a second session
    establishes and carries reports (§2 Cooldown -> Accepting)."""
    blooter = t.start_blooter()

    first = t.connected_host()
    first.drain()
    blooter.key(KEY_A, True)
    assert_report(first.recv_report(), keyboard_report(keys=[4]),
                  "first session: 'a' press")
    first.close()

    # Second session on the same blooter process.
    second = t.connected_host()
    second.drain()
    blooter.key(KEY_B, True)
    assert_report(second.recv_report(), keyboard_report(keys=[5]),
                  "second session: 'b' press")


# --------------------------------------------------------------------------
# Report forwarding (design/ARCH.md §5, §7)
# --------------------------------------------------------------------------

@tests.test
def test_keyboard_press_and_release(t):
    """Input events reach the host as HID keyboard reports on the interrupt
    channel -- the whole pipeline, FIFO to L2CAP."""
    blooter = t.start_blooter()
    host = t.connected_host()
    host.drain()

    # 'a' -> usage 4 in the first key slot.
    blooter.key(KEY_A, True)
    assert_report(host.recv_report(), keyboard_report(keys=[4]), "'a' pressed")

    # 'b' held alongside -> usage 5 appended.
    blooter.key(KEY_B, True)
    assert_report(host.recv_report(), keyboard_report(keys=[4, 5]),
                  "'a' and 'b' held")

    # Releasing 'a' leaves 'b' pressed.
    blooter.key(KEY_A, False)
    assert_report(host.recv_report(), keyboard_report(keys=[5]),
                  "'a' released, 'b' still held")

    blooter.key(KEY_B, False)
    assert_report(host.recv_report(), keyboard_report(), "all keys released")


@tests.test
def test_mouse_movement_and_buttons(t):
    """Relative motion and button state arrive as HID mouse reports."""
    blooter = t.start_blooter()
    host = t.connected_host()
    host.drain()

    blooter.rel(REL_X, 10)
    assert_report(host.recv_report(), mouse_report(x=10), "mouse moved +10 on X")

    blooter.rel(REL_Y, -5)
    assert_report(host.recv_report(), mouse_report(y=-5), "mouse moved -5 on Y")

    blooter.key(BTN_LEFT, True)
    assert_report(host.recv_report(), mouse_report(buttons=0x01),
                  "left button pressed")

    blooter.key(BTN_LEFT, False)
    assert_report(host.recv_report(), mouse_report(buttons=0x00),
                  "left button released")


@tests.test
def test_reports_only_after_connect(t):
    """Input arriving while no host is connected must not be buffered and
    replayed into the next session (main_loop drains stale input on connect)."""
    blooter = t.start_blooter()

    # Type before anyone is connected.
    blooter.key(KEY_A, True)
    blooter.key(KEY_A, False)

    host = t.connected_host()
    stale = host.drain(settle=0.6)
    assert not stale, f"got replayed reports from before the session: {stale}"

    # The session itself still works.
    blooter.key(KEY_B, True)
    assert_report(host.recv_report(), keyboard_report(keys=[5]),
                  "'b' press in the new session")


# --------------------------------------------------------------------------
# Control channel (design/ARCH.md §4)
# --------------------------------------------------------------------------

@tests.test
def test_unsupported_control_request_is_rejected(t):
    """GET_REPORT and friends get HANDSHAKE ERR_UNSUPPORTED_REQUEST (0x03),
    rather than being ignored (handle_control in classic.rs)."""
    t.start_blooter()
    host = t.connected_host()

    # GET_REPORT: transaction type 0x4 -- neither HANDSHAKE, HID_CONTROL nor DATA.
    host.send_ctrl(bytes([0x40]))
    host.ctrl.settimeout(5.0)
    reply = host.ctrl.recv(64)
    assert reply == bytes([0x03]), f"expected ERR_UNSUPPORTED_REQUEST, got {reply!r}"


@tests.test
def test_virtual_cable_unplug_ends_session(t):
    """An unplug from the host ends the session and drops blooter's bond, so
    neither side is left half-bonded (§7)."""
    blooter = t.start_blooter()
    host = t.connected_host()

    host.unplug()
    blooter.wait_for_output(
        r"unplugged us", 10.0, "blooter to notice the virtual-cable unplug")
    # The bond must go with it: a bond left on one side only makes both
    # reconnect directions fail.
    blooter.wait_for_output(
        r"removed our bond", 10.0, "blooter to drop its bond")
    # The session ends and the accept loop resumes -- an unplug is not fatal.
    blooter.wait_for_output(
        r"host disconnected", 10.0, "the session to end")
    assert blooter.proc.alive(), "blooter exited on unplug instead of accepting again"

    # Reconnecting *after* an unplug is not asserted here: it needs a fresh
    # pairing, which this stack cannot drive while blooter's agent is
    # registered (see README.md, "What this suite does not cover").


def main():
    import harness

    if len(sys.argv) < 3:
        print("usage: test_connection.py <btvirt> <blooter> [name-filter]",
              file=sys.stderr)
        return 2
    btvirt, binary = sys.argv[1], sys.argv[2]
    only = sys.argv[3] if len(sys.argv) > 3 else None

    stack = harness.Stack(btvirt)
    try:
        try:
            stack.start()
        except harness.HarnessError as exc:
            print(f"\nSTACK SETUP FAILED: {exc}", file=sys.stderr, flush=True)
            stack.dump_logs()
            return 2
        return tests.run(stack, binary, only=only)
    finally:
        stack.stop()


if __name__ == "__main__":
    sys.exit(main())
