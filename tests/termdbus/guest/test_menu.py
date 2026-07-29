#!/usr/bin/env python3
"""Tests for blooter's interactive host menu and pairing prompt.

The menu is real crossterm output on a real PTY; the Bluetooth stack under it is
mocked, so device lists and pairing requests are exactly what each test says
they are. See design/CONNECTION.md §5 (pairing) and §6 (menu).
"""

import sys

from harness import (
    APPEARANCE_COMPUTER,
    APPEARANCE_KEYBOARD,
    APPEARANCE_SPEAKER,
    CLASS_COMPUTER,
    CLASS_COMPUTER_AUDIO,
    CLASS_HEADSET,
    CLASS_TV,
    Registry,
    assert_screen_contains,
    assert_menu_contains,
    assert_menu_lacks,
    assert_screen_lacks,
    menu_rows,
    selected_index,
    wait_for,
    wait_for_menu,
)

tests = Registry()

LAPTOP = "AA:BB:CC:DD:EE:01"
DESKTOP = "AA:BB:CC:DD:EE:02"
HEADSET = "AA:BB:CC:DD:EE:03"
NAMELESS = "AA:BB:CC:DD:EE:04"


# --------------------------------------------------------------------------
# Listing and the "Other devices" split (§6)
# --------------------------------------------------------------------------

@tests.test
def test_menu_lists_discovered_hosts(t):
    """Plausible HID hosts appear in the main list, with address and alias."""
    t.mock.add_device(LAPTOP, "my-laptop", cls=CLASS_COMPUTER)
    term = t.menu()

    assert_screen_contains(term, "Bluetooth hosts:", "menu title")
    assert_screen_contains(term, LAPTOP, "the device address")
    assert_screen_contains(term, "my-laptop", "the device alias")
    assert_screen_contains(term, "[unpaired", "the unpaired status marker")
    assert_screen_contains(term, "[q] Skip", "the footer")


@tests.test
def test_paired_device_shows_paired_and_fix(t):
    """A bonded host is marked paired, and `[f] Fix connection` is offered when
    the cursor is on it (§7)."""
    t.mock.add_device(LAPTOP, "my-laptop", paired=True)
    term = t.menu()

    assert_screen_contains(term, "[paired", "the paired status marker")
    assert_screen_contains(term, "[f] Fix connection", "the fix action")


@tests.test
def test_unpaired_device_offers_no_fix(t):
    """`[f]` applies to bonded hosts only, so it must not be offered for an
    unpaired one."""
    t.mock.add_device(LAPTOP, "my-laptop", paired=False)
    term = t.menu()

    assert_menu_lacks(term, "[f] Fix connection",
                      "fix offered for an unpaired device")


@tests.test
def test_audio_device_moves_to_other_devices(t):
    """A headset (major class 4) is not a plausible HID host, so it belongs in
    the 'Other devices' submenu rather than the main list."""
    t.mock.add_device(LAPTOP, "my-laptop", cls=CLASS_COMPUTER)
    t.mock.add_device(HEADSET, "my-headset", cls=CLASS_HEADSET)
    term = t.menu()

    assert_screen_contains(term, "my-laptop", "the laptop in the main list")
    assert_screen_lacks(term, "my-headset", "the headset in the main list")
    assert_menu_contains(term, "[o] Other devices (1)", "the submenu offer")


@tests.test
def test_tv_and_audio_capable_computer_stay_on_main(t):
    """Sharing the Audio/Video major class with headsets does not make a TV a
    headset, and a laptop advertising A2DP is still a laptop -- only the headset
    here belongs in the submenu."""
    t.mock.add_device(LAPTOP, "my-laptop", cls=CLASS_COMPUTER_AUDIO)
    t.mock.add_device(DESKTOP, "my-tv", cls=CLASS_TV)
    t.mock.add_device(HEADSET, "my-headset", cls=CLASS_HEADSET)
    term = t.menu()

    assert_screen_contains(term, "my-laptop", "the A2DP laptop in the main list")
    assert_screen_contains(term, "my-tv", "the TV in the main list")
    assert_screen_lacks(term, "my-headset", "the headset in the main list")
    assert_menu_contains(term, "[o] Other devices (1)", "the submenu offer")


@tests.test
def test_paired_device_is_never_other(t):
    """Bonding a device was a deliberate choice, so it stays on the main list
    whatever its class says it is."""
    t.mock.add_device(HEADSET, "my-headset", cls=CLASS_HEADSET, paired=True)
    term = t.menu()

    assert_screen_contains(term, "my-headset", "the paired headset in the main list")
    assert_menu_lacks(term, "[o] Other devices", "a submenu offer")


@tests.test
def test_nameless_device_moves_to_other_devices(t):
    """A device with no Name property has only a hex identifier to show, so it
    is filed under 'Other devices' too."""
    t.mock.add_device(LAPTOP, "my-laptop")
    t.mock.add_device(NAMELESS, "whatever", named=False)
    term = t.menu()

    assert_menu_contains(term, "[o] Other devices (1)",
                         "the nameless device counted as other")
    assert_screen_lacks(term, NAMELESS, "the nameless device in the main list")


@tests.test
def test_other_devices_submenu_navigation(t):
    """`[o]` opens the submenu and `[b]` comes back."""
    t.mock.add_device(LAPTOP, "my-laptop")
    t.mock.add_device(HEADSET, "my-headset", cls=CLASS_HEADSET)
    term = t.menu()

    term.press("o")
    wait_for_menu(term, "Other devices:")
    assert_menu_contains(term, "my-headset", "the headset in the submenu")
    assert_menu_contains(term, "[b] Back", "the back action")

    term.press("b")
    wait_for_menu(term, "Bluetooth hosts:")
    assert_menu_contains(term, "my-laptop", "the main list after going back")


# --------------------------------------------------------------------------
# Navigation
# --------------------------------------------------------------------------

@tests.test
def test_arrow_keys_move_the_cursor(t):
    """Down/Up move the '>' marker between rows.

    Asserted on row numbers, not device names: blooter lists devices in whatever
    order D-Bus enumeration yields, which is not the order they were added.
    """
    t.mock.add_device(LAPTOP, "host-alpha")
    t.mock.add_device(DESKTOP, "host-beta")
    term = t.menu()

    rows = menu_rows(term.screen())
    assert len(rows) == 2, f"expected two rows, got {rows}"
    assert selected_index(term.screen()) == 1, \
        f"cursor should start on row 1:\n{term.screen()}"

    term.press("Down")
    term.wait_for_idle()
    assert selected_index(term.screen()) == 2, \
        f"Down should select row 2:\n{term.screen()}"

    term.press("Up")
    term.wait_for_idle()
    assert selected_index(term.screen()) == 1, \
        f"Up should return to row 1:\n{term.screen()}"


@tests.test
def test_cursor_stops_at_list_ends(t):
    """The cursor clamps: Up on the first row and Down on the last stay put."""
    t.mock.add_device(LAPTOP, "host-alpha")
    t.mock.add_device(DESKTOP, "host-beta")
    term = t.menu()

    term.press("Up")
    term.wait_for_idle()
    assert selected_index(term.screen()) == 1, "Up past the top should clamp"

    term.press("Down")
    term.press("Down")
    term.wait_for_idle()
    assert selected_index(term.screen()) == 2, "Down past the bottom should clamp"


@tests.test
def test_rescan_redraws_the_menu(t):
    """`[r]` rescans; a device added meanwhile shows up afterwards."""
    t.mock.add_device(LAPTOP, "my-laptop")
    term = t.menu()
    assert_screen_lacks(term, "late-arrival", "the device before it exists")

    t.mock.add_device(DESKTOP, "late-arrival")
    term.press("r")
    term.wait_for_text("late-arrival")


@tests.test
def test_skip_closes_the_menu(t):
    """`[q]` skips host selection and leaves blooter accepting."""
    t.mock.add_device(LAPTOP, "my-laptop")
    term = t.menu()

    term.press("q")
    term.wait_for_text("ready to accept connections")
    assert term.running(), "blooter exited when the menu was skipped"


@tests.test
def test_menu_handles_no_devices(t):
    """With nothing discovered the menu still renders, with '(none found)'."""
    term = t.menu()
    assert_screen_contains(term, "(none found)", "the empty-list placeholder")
    assert_screen_contains(term, "[q] Skip", "the footer with no devices")


# --------------------------------------------------------------------------
# Pairing prompt and the terminal hand-off (§5)
# --------------------------------------------------------------------------

@tests.test
def test_incoming_pairing_prompt_interrupts_menu(t):
    """The TermCoord hand-off: an inbound RequestConfirmation while the menu is
    live in raw mode must suspend the menu, print the prompt, and read the reply
    from the same stdin -- otherwise the menu's EventStream swallows it and
    pairing stalls (design/CONNECTION.md §5.2)."""
    t.mock.add_device(LAPTOP, "my-laptop")
    term = t.menu(pairing="prompt")

    call = t.mock.request_confirmation(LAPTOP, passkey=123456)

    # The prompt must reach the terminal even though the menu owns it.
    term.wait_for_text("Confirm pairing with")
    assert_screen_contains(term, "[Y/n]", "the prompt's answer hint")

    # And the reply must be read by the prompt, not eaten by the menu.
    term.type("y")
    term.press("Enter")
    assert call.accepted(), "blooter did not accept the pairing"


@tests.test
def test_pairing_prompt_rejects_on_no(t):
    """Answering 'n' rejects the pairing with org.bluez.Error.Rejected."""
    term = t.menu(pairing="prompt")

    call = t.mock.request_authorization(LAPTOP)

    term.wait_for_text("Allow pairing with")
    term.type("n")
    term.press("Enter")

    assert not call.accepted(), "blooter accepted a pairing the user declined"
    assert call.error_name() == "org.bluez.Error.Rejected", \
        f"unexpected error name: {call.error_name()}"


@tests.test
def test_menu_resumes_after_pairing_prompt(t):
    """Once the prompt is answered the menu comes back, fully repainted, and
    still responds to keys -- the borrow guard must restore raw mode and a new
    EventStream (§5.2)."""
    t.mock.add_device(LAPTOP, "my-laptop")
    t.mock.add_device(HEADSET, "my-headset", cls=CLASS_HEADSET)
    term = t.menu(pairing="prompt")

    call = t.mock.request_authorization(LAPTOP)
    term.wait_for_text("Allow pairing with")
    term.type("y")
    term.press("Enter")
    assert call.accepted(), "pairing was not accepted"

    # The menu must be back...
    wait_for_menu(term, "Bluetooth hosts:")
    # ...and still driving keys, which it cannot do without a live EventStream.
    term.press("o")
    wait_for_menu(term, "Other devices:")


# --------------------------------------------------------------------------
# The BLE menu (§4, §6)
#
# Same menu, same rendering, driven by the LE transport. The devices here carry
# no Class of Device -- as an LE-only peer does not -- so the "Other devices"
# split has to come from GAP Appearance instead.
# --------------------------------------------------------------------------

@tests.test
def test_ble_menu_lists_discovered_hosts(t):
    """The menu opens on BLE too, with the same title, rows and footer."""
    t.mock.add_device(LAPTOP, "my-laptop", cls=None,
                      appearance=APPEARANCE_COMPUTER)
    term = t.menu(protocol="ble")

    assert_screen_contains(term, "Bluetooth hosts:", "menu title")
    assert_screen_contains(term, LAPTOP, "the device address")
    assert_screen_contains(term, "my-laptop", "the device alias")
    assert_screen_contains(term, "[q] Skip", "the footer")


@tests.test
def test_ble_appearance_moves_peripherals_to_other_devices(t):
    """With no Class of Device to go on, a keyboard (Appearance category 0x0F)
    and a speaker (0x21) are filed under 'Other devices' by Appearance."""
    t.mock.add_device(LAPTOP, "my-laptop", cls=None,
                      appearance=APPEARANCE_COMPUTER)
    t.mock.add_device(HEADSET, "my-speaker", cls=None,
                      appearance=APPEARANCE_SPEAKER)
    t.mock.add_device(DESKTOP, "some-keyboard", cls=None,
                      appearance=APPEARANCE_KEYBOARD)
    term = t.menu(protocol="ble")

    assert_screen_contains(term, "my-laptop", "the laptop in the main list")
    assert_screen_lacks(term, "my-speaker", "the speaker in the main list")
    assert_screen_lacks(term, "some-keyboard", "the keyboard in the main list")
    assert_menu_contains(term, "[o] Other devices (2)", "the submenu offer")


@tests.test
def test_ble_offers_fix_for_a_bonded_host(t):
    """A BLE host caches blooter's GATT database — the Report Map with it —
    across its bond, and `[f] Fix connection` invalidates that with a Service
    Changed indication, so it is offered here too (design/CONNECTION.md §7)."""
    t.mock.add_device(LAPTOP, "my-laptop", cls=None, paired=True,
                      appearance=APPEARANCE_COMPUTER)
    term = t.menu(protocol="ble")

    assert_screen_contains(term, "[paired", "the paired status marker")
    assert_screen_contains(term, "[f] Fix connection", "the fix action")


@tests.test
def test_ble_pick_pairs_then_connects(t):
    """Selecting an unbonded host bonds it from here and then initiates the
    outgoing LE connection (design/CONNECTION.md §4, §6)."""
    path = t.mock.add_device(LAPTOP, "my-laptop", cls=None, paired=False,
                             appearance=APPEARANCE_COMPUTER)
    term = t.menu(protocol="ble")

    term.press("Enter")
    term.wait_for_text("Pairing with my-laptop")
    wait_for(lambda: "Connect" in t.mock.calls(path), 15.0,
             "blooter to pair and then connect to the picked host")

    calls = t.mock.calls(path)
    assert calls.index("Pair") < calls.index("Connect"), \
        f"expected Pair before Connect, got {calls}"


@tests.test
def test_ble_skip_closes_the_menu(t):
    """`[q]` skips, leaving blooter advertising and waiting to be subscribed."""
    t.mock.add_device(LAPTOP, "my-laptop", cls=None,
                      appearance=APPEARANCE_COMPUTER)
    term = t.menu(protocol="ble")

    term.press("q")
    term.wait_for_text("ready to accept connections")
    assert term.running(), "blooter exited when the BLE menu was skipped"


def main():
    import harness

    if len(sys.argv) < 2 or not sys.argv[1]:
        only = None
    else:
        only = sys.argv[1]

    mock = harness.MockBluez()
    try:
        try:
            mock.start()
        except harness.HarnessError as exc:
            print(f"\nMOCK SETUP FAILED: {exc}", file=sys.stderr, flush=True)
            mock.dump_logs()
            return 2
        return tests.run(mock, only=only)
    finally:
        mock.stop()


if __name__ == "__main__":
    sys.exit(main())
