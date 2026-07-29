#!/usr/bin/env python3
"""End-to-end tests for blooter's connection logic.

Every test drives the real binary over a real link from a second emulated
controller — L2CAP on Classic, ATT/GATT on BLE. See harness.py for the stack
and design/CONNECTION.md for the behaviour being pinned down.

Both transports live here rather than in separate files because the stack
(btvirt + bluetoothd) is started once and shared by the whole run.
"""

import sys

from harness import (
    BTN_LEFT,
    CONSUMER_TV,
    CONSUMER_VOLUME_UP,
    KEY_A,
    KEY_B,
    KEY_LEFTMETA,
    KEY_T,
    KEY_VOLUMEUP,
    REL_X,
    REL_Y,
    Registry,
    assert_report,
    consumer_report,
    describe,
    keyboard_report,
    le_payload,
    mouse_report,
    wait_for,
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

    # Both axes of one frame belong in a single report, not one per axis.
    blooter.rel_frame((REL_X, 3), (REL_Y, -4))
    assert_report(host.recv_report(), mouse_report(x=3, y=-4),
                  "one frame with both axes is one report")

    blooter.key(BTN_LEFT, True)
    assert_report(host.recv_report(), mouse_report(buttons=0x01),
                  "left button pressed")

    blooter.key(BTN_LEFT, False)
    assert_report(host.recv_report(), mouse_report(buttons=0x00),
                  "left button released")


@tests.test
def test_pointer_motion_is_batched(t):
    """With batching on, many small frames arriving faster than the flush
    interval collapse into one larger report rather than a queue of small ones
    (design/ARCH.md §7.2c)."""
    blooter = t.start_blooter(batch=50)
    host = t.connected_host()
    host.drain()

    # Twenty frames of +2, written back to back and so well inside the 50 ms
    # interval. Their sum fits in one report's signed 8-bit range.
    for _ in range(20):
        blooter.rel(REL_X, 2)

    reports = host.drain(settle=1.0)
    assert reports, "no mouse reports arrived"
    for r in reports:
        assert r[:3] == mouse_report()[:3], f"not a mouse report: {describe(r)}"

    # Batching must not lose motion: the pointer travels exactly as far as it
    # was moved, however the frames were grouped.
    travelled = sum(int.from_bytes(r[3:4], "big", signed=True) for r in reports)
    assert travelled == 40, (
        f"expected 40 counts of travel, got {travelled} "
        f"from {[describe(r) for r in reports]}")

    # ...and it must actually batch. The first frame after an idle period goes
    # out immediately (the interval has long since elapsed), so the twenty
    # frames become a handful of reports, not twenty.
    assert len(reports) < 20, (
        f"nothing was merged: {len(reports)} reports for 20 frames")


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


# --------------------------------------------------------------------------
# BLE / HOGP (design/CONNECTION.md §4, design/ARCH.md §4.2)
#
# The same input pipeline over the LE transport: blooter advertises its HOGP
# GATT tree, the host subscribes to the Report CCCDs -- which is what counts as
# connected -- and reports arrive as notifications instead of interrupt-channel
# writes.
# --------------------------------------------------------------------------

@tests.test
def test_ble_advertises_hogp_gatt_tree(t):
    """blooter's HID service is discoverable over LE, with one Report
    characteristic per report id (mouse and keyboard here)."""
    t.start_blooter(protocol="ble")
    host = t.le_host().connect()

    handles = host.report_handles()
    assert len(handles) == 2, \
        f"expected mouse + keyboard Report characteristics, got {handles}"

    # The layout characteristic puts the descriptor fingerprint where the GATT
    # Database Hash covers it, so a host caching the Report Map can see a
    # descriptor change (design/CONNECTION.md §7.2b). Its UUID tail is the
    # fingerprint, which is not fixed here — only that it is non-zero, i.e. that
    # a real fingerprint was registered.
    uuid = host.layout_uuid()
    assert uuid, f"no layout characteristic in blooter's GATT tree:\n{host.output()}"
    assert uuid.split("-")[-1][-8:] != "00000000", \
        f"layout characteristic carries no fingerprint: {uuid}"


@tests.test
def test_ble_subscribe_connects(t):
    """A CCCD subscribe is the LE 'connect': blooter reports the host connected
    once the first Report characteristic is subscribed to."""
    t.start_blooter(protocol="ble")
    t.connected_le_host()


@tests.test
def test_ble_keyboard_press_and_release(t):
    """Key events reach the host as notifications, with the HIDP header and
    report id stripped."""
    t.start_blooter(protocol="ble")
    host = t.connected_le_host()

    t.blooter.key(KEY_A, True)
    host.wait_for_notification(le_payload(keyboard_report(keys=[0x04])))

    t.blooter.key(KEY_A, False)
    host.wait_for_notification(le_payload(keyboard_report()))


@tests.test
def test_ble_mouse_movement_and_buttons(t):
    """Relative motion and a button press notify on the mouse Report
    characteristic."""
    t.start_blooter(protocol="ble")
    host = t.connected_le_host()

    t.blooter.rel(REL_X, 5)
    host.wait_for_notification(le_payload(mouse_report(x=5)))

    t.blooter.rel(REL_Y, -3)
    host.wait_for_notification(le_payload(mouse_report(y=-3)))

    t.blooter.key(BTN_LEFT, True)
    host.wait_for_notification(le_payload(mouse_report(buttons=0x01)))


@tests.test
def test_ble_unsubscribe_ends_session(t):
    """Dropping the last subscription ends the session and returns blooter to
    advertising, ready for the next host (§4)."""
    t.start_blooter(protocol="ble")
    host = t.connected_le_host()

    host.close()
    t.blooter.wait_for_output(
        r"host disconnected", 10.0,
        "blooter to end the session after the host left")
    assert t.blooter.proc.alive(), "blooter exited when the LE host left"


# --------------------------------------------------------------------------
# TV remote (design/REMOTE.md)
# --------------------------------------------------------------------------

# The remote is off by default precisely so it costs existing bonds nothing, so
# every test below has to turn it on explicitly.
REMOTE_CONFIG = '[remote]\nenabled = true\ntv = "leftmeta+t"\n'


def consumer_payloads(host):
    """The consumer notifications received so far, in order.

    Identified by length: the consumer report's value is 2 bytes, where the
    mouse's is 4 and the keyboard's 9 (design/REMOTE.md §4).
    """
    return [p for _handle, p in host.notifications() if len(p) == 2]


def wait_for_consumer_tail(host, expected, timeout=5.0):
    """Wait until the last len(expected) consumer notifications are `expected`.

    A tail rather than a `wait_for_notification` per report: a release is
    `00 00` whatever was released, so matching one anywhere in the history
    would be satisfied by an earlier release and prove nothing about order.
    """
    want = [le_payload(r) for r in expected]

    def check():
        return consumer_payloads(host)[-len(want):] == want

    try:
        wait_for(check, timeout, f"consumer reports {[describe(p) for p in want]}")
    except Exception:
        got = [describe(p) for p in consumer_payloads(host)]
        raise AssertionError(
            f"expected the last consumer reports to be "
            f"{[describe(p) for p in want]}\n  received: {got or '<none>'}") from None


@tests.test
def test_ble_remote_advertises_a_consumer_report(t):
    """With [remote] enabled the GATT tree gains a third Report characteristic,
    for the consumer collection (design/REMOTE.md §3.1, §8)."""
    t.start_blooter(protocol="ble", config_extra=REMOTE_CONFIG)
    host = t.le_host().connect()

    handles = host.report_handles()
    assert len(handles) == 3, \
        f"expected mouse + keyboard + consumer Report characteristics, got {handles}"


@tests.test
def test_ble_remote_off_by_default(t):
    """Without the section there is no consumer characteristic at all — the
    guarantee that makes the feature free for everyone else (§3.1)."""
    t.start_blooter(protocol="ble")
    host = t.le_host().connect()

    assert len(host.report_handles()) == 2, \
        "an unconfigured blooter must advertise no consumer collection"


@tests.test
def test_ble_media_key_passthrough(t):
    """A media key the keyboard collection cannot carry reaches the host on the
    consumer one instead (§5)."""
    t.start_blooter(protocol="ble", config_extra=REMOTE_CONFIG)
    host = t.connected_le_host()

    t.blooter.key(KEY_VOLUMEUP, True)
    wait_for_consumer_tail(host, [consumer_report(CONSUMER_VOLUME_UP)])
    t.blooter.key(KEY_VOLUMEUP, False)
    wait_for_consumer_tail(host, [consumer_report(CONSUMER_VOLUME_UP),
                                  consumer_report()])


@tests.test
def test_ble_remote_chord_is_a_tap(t):
    """A [remote] chord produces exactly two frames — press then release — and
    forwards neither of its own keys (§6)."""
    t.start_blooter(protocol="ble", config_extra=REMOTE_CONFIG)
    host = t.connected_le_host()

    # Anchor the count on a report already waited for, so the zeroed report
    # blooter pushes at connect cannot still be in flight while counting.
    t.blooter.key(KEY_VOLUMEUP, True)
    t.blooter.key(KEY_VOLUMEUP, False)
    wait_for_consumer_tail(host, [consumer_report(CONSUMER_VOLUME_UP),
                                  consumer_report()])
    before = len(consumer_payloads(host))

    t.blooter.key(KEY_LEFTMETA, True)
    t.blooter.key(KEY_T, True)
    wait_for_consumer_tail(host, [consumer_report(CONSUMER_TV), consumer_report()])
    t.blooter.key(KEY_T, False)
    t.blooter.key(KEY_LEFTMETA, False)

    # Releasing the chord's keys adds nothing: the host never saw them go down,
    # and the tap does not autorepeat.
    t.blooter.key(KEY_VOLUMEUP, True)
    wait_for_consumer_tail(host, [consumer_report(CONSUMER_VOLUME_UP)])
    assert len(consumer_payloads(host)) == before + 3, \
        f"a tap plus one press is three reports, got "\
        f"{[describe(p) for p in consumer_payloads(host)[before:]]}"
    # Nor did the chord's keys reach the keyboard collection.
    assert le_payload(keyboard_report(modifiers=0x08)) not in \
        [p for _h, p in host.notifications()], \
        "a fired chord must not forward its own keys"


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
