"""The core journeys: design/TESTS.md §5.

Every one starts from genuinely unpaired adapters. There is no `btmgmt pair`
preamble anywhere in this suite -- that preamble is precisely what
`tests/btvirt` has to do, and precisely why the bond is the one thing it can
never test.

What "working" means here is the full round trip (§4): an `input_event` injected
on `dev`, encoded by blooter, carried over the real link, parsed by the **host
kernel's** HID stack into input devices, and read back as evdev events on the
host. Not "the right bytes went out" but "a stock Linux host ended up with a
working keyboard and mouse".
"""

from common import (
    KEY_A,
    KEY_B,
    KEY_LEFTSHIFT,
    KEY_RIGHTSHIFT,
    REL_X,
    REL_Y,
)

from . import both_transports, tests


# --------------------------------------------------------------------------
# J1 -- cold pair to working input
# --------------------------------------------------------------------------

@both_transports
def test_cold_pair_to_working_input(t, protocol):
    """Unpaired adapters -> host initiates -> bond on both sides -> the host
    builds HID devices -> injected keys and motion arrive.

    This is the test that would have caught the `AuthenticationFailed` bug
    (CONNECTION.md §5): it is exactly "from unpaired adapters to a working
    keyboard and mouse", and it fails loudly if blooter's agent refuses its own
    pairing. All 18 btvirt tests passed while that bug shipped.
    """
    t.start_blooter(protocol=protocol)

    assert not t.dev.bonds(), "dev started with a bond it should not have"
    assert not t.host.bonds(), "host started with a bond it should not have"

    t.pair_to_working_input()

    # Both halves of the bond, on the transport that was configured. A bond on
    # one side only is the whole subject of §6, so its absence is asserted here
    # rather than assumed.
    dev_bond = t.dev.bonded_to(t.host.address)
    host_bond = t.host.bonded_to(t.dev.address)
    assert dev_bond, f"dev holds no bond for the host: {t.dev.bonds()}"
    assert host_bond, f"host holds no bond for dev: {t.host.bonds()}"
    key = "ltk" if protocol == "ble" else "linkkey"
    assert dev_bond[key], (
        f"dev's bond for the host is not a {protocol} bond: {dev_bond}")

    t.expect_key(KEY_A)
    t.expect_key(KEY_B)
    t.expect_motion(REL_X, 7)
    t.expect_motion(REL_Y, -4)


# --------------------------------------------------------------------------
# J2 -- disconnect and reconnect
# --------------------------------------------------------------------------

@both_transports
def test_disconnect_then_reconnect(t, protocol):
    """The host drops the link and reconnects with no re-pairing, and input
    works again."""
    t.start_blooter(protocol=protocol)
    t.pair_to_working_input()

    # No wait for blooter to *log* the disconnect: on BLE it ends a session on
    # the CCCD unsubscribe, and a link that simply drops under an idle session
    # need not produce a line at all. What J2 is asking is whether the host can
    # come back, so that is what is waited for -- a blooter still wedged in the
    # old session fails here, which is the finding either way.
    t.reconnect_host()

    # No re-pairing happened: both bonds are the ones from the first session.
    assert t.dev.bonded_to(t.host.address), "dev lost its bond over a reconnect"
    assert t.host.bonded_to(t.dev.address), "host lost its bond over a reconnect"

    t.expect_working_input()


# --------------------------------------------------------------------------
# J3 -- blooter restarts
# --------------------------------------------------------------------------

@both_transports
def test_blooter_restart(t, protocol):
    """`dev` restarts blooter and the host reconnects -- to the advertisement on
    BLE, by dialing on Classic -- without either side re-pairing."""
    t.start_blooter(protocol=protocol)
    t.pair_to_working_input()

    t.dev.stop_blooter()
    t.start_blooter(protocol=protocol)

    # The bond outlives the process on both sides: it lives in bluetoothd, not
    # in blooter.
    assert t.dev.bonded_to(t.host.address), \
        "dev's bond did not survive a blooter restart"

    t.reconnect_host()
    t.expect_working_input()


# --------------------------------------------------------------------------
# J4 -- the host stack goes away and comes back
# --------------------------------------------------------------------------

@both_transports
def test_host_stack_restart(t, protocol):
    """The bond survives on both sides and input works once the host is back.

    `restart_stack` restarts the host's bluetoothd over its persisted bond
    storage rather than rebooting the VM -- vng runs a guest for exactly one
    command, so a real reboot would end it. The persisted state is what a reboot
    would have carried across, so what J4 asks is fully exercised; what is not
    covered is anything the *kernel* would forget, which README.md states as a
    gap.
    """
    t.start_blooter(protocol=protocol)
    t.pair_to_working_input()

    t.host.restart_stack()
    assert t.host.bonded_to(t.dev.address), \
        "the host's bond did not survive its stack restarting"
    assert t.dev.bonded_to(t.host.address), \
        "dev's bond did not survive the host's stack restarting"

    t.reconnect_host()
    t.expect_working_input()


# --------------------------------------------------------------------------
# J5 -- capture toggle, over the evdev path (uinput)
# --------------------------------------------------------------------------

KEYBOARD_KEYS = [KEY_A, KEY_B, KEY_LEFTSHIFT, KEY_RIGHTSHIFT]
MOUSE_RELS = [REL_X, REL_Y]


@tests.test
def test_capture_toggle_grabs_and_releases(t):
    """`-x` grabs on connect, releases on capture-off, and re-grabs on
    capture-on.

    The only test here that uses uinput rather than the FIFO, because the
    exclusive grab is invisible to FIFO mode -- and `-x` capture is where a
    user-reported symptom landed (design/TESTS.md §4.1).

    The grab is asserted against the kernel, not against a log line: a second
    reader on the same device sees nothing at all while blooter holds
    `EVIOCGRAB`, and sees the identical injection once the grab is dropped.
    """
    devices = t.dev.make_uinput(KEYBOARD_KEYS, [], MOUSE_RELS)
    t.start_blooter(protocol="classic", fifo=False,
                    extra_args=["-x", "-e", str(devices["keyboard"]),
                                "-e", str(devices["mouse"])])
    # The same "pair, then connect, then wait for the devices" as everywhere
    # else: bluetoothctl drops the link once bonding is done, so pairing alone
    # never leaves a session behind.
    t.pair_to_working_input()

    # While captured, a rival reader on the keyboard sees nothing...
    t.dev.watch_input("keyboard")
    t.dev.uinput_key("keyboard", KEY_A, True)
    t.dev.uinput_key("keyboard", KEY_A, False)
    leaked = [e for e in t.dev.watch_read(timeout=1.0) if e[0] == 0x01]
    assert not leaked, \
        f"blooter is not holding an exclusive grab; a rival reader saw {leaked}"

    # ...and the host does, which is what the grab is for.
    got = t.host.read_events(timeout=5.0)
    assert (t.KEYBOARD, 0x01, KEY_A, 1) in got, \
        f"the captured key never reached the host: {got}"

    # Capture off (the default chord: Left Shift then Right Shift).
    t.dev.uinput_key("keyboard", KEY_LEFTSHIFT, True)
    t.dev.uinput_key("keyboard", KEY_RIGHTSHIFT, True)
    t.dev.uinput_key("keyboard", KEY_RIGHTSHIFT, False)
    t.dev.uinput_key("keyboard", KEY_LEFTSHIFT, False)
    t.dev.wait_log(r"[Cc]apture off|not capturing|capture disabled", timeout=15.0,
                   what="blooter to report capture off")

    # The grab is gone: the same injection now reaches the rival reader.
    t.dev.uinput_key("keyboard", KEY_B, True)
    t.dev.uinput_key("keyboard", KEY_B, False)
    released = [e for e in t.dev.watch_read(timeout=2.0)
                if e[0] == 0x01 and e[1] == KEY_B]
    assert released, \
        "blooter did not release its grab when capture was turned off"

    # Capture on again re-grabs.
    t.dev.uinput_key("keyboard", KEY_LEFTSHIFT, True)
    t.dev.uinput_key("keyboard", KEY_RIGHTSHIFT, True)
    t.dev.uinput_key("keyboard", KEY_RIGHTSHIFT, False)
    t.dev.uinput_key("keyboard", KEY_LEFTSHIFT, False)
    t.dev.wait_log(r"[Cc]apture on|capturing|capture enabled", timeout=15.0,
                   what="blooter to report capture on")

    # Drain first: the chord's own keys were injected while the grab was
    # released, so they are legitimately sitting in the rival reader's queue and
    # would otherwise be read as a leak from the injection below.
    t.dev.watch_read(timeout=0.5)

    t.dev.uinput_key("keyboard", KEY_A, True)
    t.dev.uinput_key("keyboard", KEY_A, False)
    leaked = [e for e in t.dev.watch_read(timeout=1.0) if e[0] == 0x01]
    assert not leaked, \
        f"blooter did not re-grab when capture was turned back on: {leaked}"
