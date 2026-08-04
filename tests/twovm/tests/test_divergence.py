"""Divergence and recovery: design/TESTS.md §6.

The point of the suite. Each scenario damages one or both bond stores **out of
band** -- directly via `bluetoothctl` on whichever side, never through blooter's
own menu -- and then asks whether the user can get back to a working keyboard.

Three things are asserted per row, and the third is the one that matters:

1. **The symptom**, so a regression that changes the failure mode is caught.
2. **Detection** -- blooter notices and says which host and which problem
   (CONNECTION.md §8.2). Those assertions are marked `xfail` (§6.1): §8.2 is a
   design commitment that is not yet fully implemented, and writing them now
   makes the section executable rather than aspirational. An xfail that starts
   passing is how the work gets recognised as done.
3. **The remedy works** -- perform exactly the steps blooter printed and end at
   a working keyboard and mouse. *This is the assertion that keeps blooter's
   advice honest*: a message telling the user to do something that does not fix
   it is a bug, and nothing else in the project can catch it.
"""

from common import HarnessError, KEY_A, REL_X, REL_Y, wait_for

from . import both_transports, tests

# The uinput devices D8 needs: `[gamepad] slots` has no effect in FIFO mode
# (main.rs forces the count to zero there), so the one row that is about the
# descriptor changing has to come in over evdev.
MOUSE_RELS = [REL_X, REL_Y]


def probe_reconnect(t, timeout=30.0):
    """Try to get back to a working session, and report what happened.

    Returns one of "refused" (the host's connect was rejected), "no session"
    (the link came up but blooter never opened one) or "recovered" (it simply
    worked). All three are real outcomes of a damaged bond store and which one a
    row gets is transport-dependent, so the rows say which they expect rather
    than the helper assuming.
    """
    sessions = t.dev.sessions()
    try:
        t.host.reconnect(t.dev.address)
    except HarnessError:
        return "refused"
    try:
        t.dev.wait_log(t.dev.CONNECTED, timeout=timeout, after=sessions)
    except HarnessError:
        return "no session"
    return "recovered"


def assert_broken(t, timeout=30.0):
    """The reconnect must not just work; return how it failed."""
    shape = probe_reconnect(t, timeout=timeout)
    if shape == "recovered":
        raise AssertionError(
            "the host reconnected and blooter opened a session, but the two "
            "sides no longer agree about the bond -- this should not have worked")
    print(f"    | symptom: {shape}", flush=True)
    return shape


def fix_key(protocol):
    """Which menu key drops blooter's half of a bond on this transport.

    BLE has `[u]` Forget host. Classic has no `[u]` at all: there, dropping our
    half is what `[f]` does on the way to unplugging the host (CONNECTION.md
    §6, §7.2a).
    """
    return "u" if protocol == "ble" else "f"


# --------------------------------------------------------------------------
# D1 -- the host forgets blooter
# --------------------------------------------------------------------------

@both_transports
def test_d1_host_removes_bond(t, protocol):
    """`bluetoothctl remove <dev>` on the host.

    The two transports genuinely differ here, and the difference is blooter's
    doing. Removing a bonded HID device on Classic makes the host send a HIDP
    virtual-cable unplug, and `run_session` drops our bond to match rather than
    leaving a one-sided one behind (CONNECTION.md §7.2a) -- so the divergence
    never forms and a plain re-pair is the whole remedy. BLE has no unplug, so
    blooter *is* left holding a bond for a host that has none, and the remedy is
    the menu's `[u]`.
    """
    t.start_blooter(protocol=protocol, interactive=True)
    t.pair_to_working_input()

    # -- the damage, out of band --
    t.host.remove(t.dev.address)
    assert not t.host.bonded_to(t.dev.address), "the host kept its bond"

    if t.dev.bonded_to(t.host.address):
        # -- the symptom --
        assert_broken(t)

        # -- the remedy blooter offers --
        t.dev.menu(fix_key(protocol))
        wait_for(lambda: not t.dev.bonded_to(t.host.address), 30.0,
                 "blooter to drop its half of the bond from the menu")
    else:
        # Nothing to repair: the host's removal reached blooter as a HIDP
        # virtual-cable unplug and `run_session` dropped our half to match
        # (§7.2a). Whether it does depends on the host sending one, which is
        # why this is a branch and not an assertion -- but a one-sided bond
        # from here *must* be repairable from the menu, which the other branch
        # is what checks.
        print("    | no divergence formed: blooter dropped its own half",
              flush=True)

    t.pair_to_working_input()
    t.expect_working_input()


@both_transports(xfail="CONNECTION.md §8.2 detection is not implemented yet")
def test_d1_detection(t, protocol):
    """blooter must name the host and the problem, not sit there saying it is
    advertising (CONNECTION.md §8.2: *a setup that cannot work should never
    present as one that is merely waiting*)."""
    t.start_blooter(protocol=protocol)
    t.pair_to_working_input()
    t.host.remove(t.dev.address)
    probe_reconnect(t)

    t.dev.wait_log(rf"(?i){t.host.address}.*(bond|pair)", timeout=30.0,
                   what="blooter to name the host whose bond went away")


# --------------------------------------------------------------------------
# D2 -- blooter forgets the host
# --------------------------------------------------------------------------

@both_transports
def test_d2_dev_removes_bond(t, protocol):
    """`bluetoothctl remove <host>` on dev.

    The mirror image of D1, and again the transports diverge. On Classic the
    host re-pairs transparently: both agents accept (`pairing = "accept"` here,
    NoInputNoOutput there), so the reconnect renegotiates a link key and the
    user sees nothing. On BLE the host reconnects with an LTK blooter no longer
    has and the link dies at encryption -- and this is the case blooter cannot
    fix from its own side at all, because a peripheral cannot reach into a
    central's settings. The remedy is therefore host-side, and what is asserted
    is that it is *sufficient*.
    """
    t.start_blooter(protocol=protocol, interactive=True)
    t.pair_to_working_input()

    t.dev.remove_bond(t.host.address)
    assert not t.dev.bonded_to(t.host.address), "dev kept its bond"
    assert t.host.bonded_to(t.dev.address), \
        "the host dropped its bond too; this row needs a one-sided bond"

    if protocol == "classic":
        shape = probe_reconnect(t)
        assert shape == "recovered", (
            "a Classic reconnect against a bond only the host holds should "
            f"re-pair through the two agents and just work, but it {shape}")
        t.attach_input()
    else:
        assert_broken(t)
        # The remedy, exactly as blooter has to phrase it: remove blooter from
        # that host's Bluetooth settings, then pair again from there.
        t.host.remove(t.dev.address)
        t.pair_to_working_input()

    t.expect_working_input()


@both_transports(xfail="CONNECTION.md §8.2 detection is not implemented yet")
def test_d2_detection(t, protocol):
    """A host that holds a key blooter no longer has must be named, with the
    host-side remedy spelled out -- §8.2's "a repair the user cannot perform
    from blooter's side must say so plainly"."""
    t.start_blooter(protocol=protocol)
    t.pair_to_working_input()
    t.dev.remove_bond(t.host.address)
    probe_reconnect(t)

    t.dev.wait_log(r"(?i)remove blooter from .*(settings|host)", timeout=30.0,
                   what="blooter to spell out the host-side remedy")


# --------------------------------------------------------------------------
# D3 -- the control row
# --------------------------------------------------------------------------

@both_transports
def test_d3_both_remove_is_a_clean_slate(t, protocol):
    """Remove on both sides: no symptom at all, a plain re-pair works.

    The control row. Without it, D1 and D2 prove only that *removal* breaks
    things; with it, they prove that the **divergence** does.
    """
    t.start_blooter(protocol=protocol)
    t.pair_to_working_input()

    t.host.remove(t.dev.address)
    t.dev.remove_bond(t.host.address)
    assert not t.dev.bonds() and not t.host.bonds(), \
        f"a bond survived on one side: dev={t.dev.bonds()} host={t.host.bonds()}"

    t.pair_to_working_input()
    t.expect_working_input()


# --------------------------------------------------------------------------
# D4 / D5 -- trust
# --------------------------------------------------------------------------

@both_transports
def test_d4_host_untrusts(t, protocol):
    """`trust off` on the host.

    On Classic this pins `AuthorizeService`: the host will refuse the service
    unless blooter's agent answers the authorization request. On BLE it should
    change nothing, which is why the row runs on both -- the BLE variant is what
    proves the Classic one is measuring what it claims to.
    """
    t.start_blooter(protocol=protocol)
    t.pair_to_working_input()

    t.host.trust(t.dev.address, False)
    # Must still work: blooter's agent authorizes the service itself (§5.1).
    t.reconnect_host()
    t.expect_working_input()


@both_transports
def test_d5_dev_untrusts(t, protocol):
    """`trust off` on dev: reconnect still works.

    Asserts that blooter does *not* require the host to be trusted -- its agent
    authorizes, so trust is never a precondition anyone has to know about.
    """
    t.start_blooter(protocol=protocol)
    t.pair_to_working_input()

    t.dev.call("shell", argv=["bluetoothctl", "untrust", t.host.address],
               timeout=30.0)
    t.reconnect_host()
    t.expect_working_input()


# --------------------------------------------------------------------------
# D6 -- dev loses everything
# --------------------------------------------------------------------------

@both_transports
def test_d6_dev_storage_wiped(t, protocol):
    """Wipe `/var/lib/bluetooth` on dev -- what a reinstall looks like.

    Every host becomes unknown and every bond one-sided at a stroke. Each host
    must be re-pairable afterwards; nothing may be permanently stranded.
    """
    t.start_blooter(protocol=protocol)
    t.pair_to_working_input()

    t.dev.stop_blooter()
    t.dev.wipe_bonds()
    assert not t.dev.bonds(), "the wipe left something behind"
    assert t.host.bonded_to(t.dev.address), \
        "the host lost its bond too; this row needs the host's half intact"
    t.start_blooter(protocol=protocol)

    assert_broken(t)

    # Re-pairable: the host removes its orphaned half and pairs again.
    t.host.remove(t.dev.address)
    t.pair_to_working_input()
    t.expect_working_input()


@tests.test(xfail="CONNECTION.md §8.2 detection is not implemented yet")
def test_d6_detection(t):
    """§8.2.1: check at startup, not on first failure. A blooter that starts
    with no bonds at all where hosts are expecting one has everything it needs
    to say so before anybody tries to connect."""
    t.start_blooter(protocol="classic")
    t.pair_to_working_input()
    t.dev.stop_blooter()
    t.dev.wipe_bonds()
    t.start_blooter(protocol="classic")

    t.dev.wait_log(r"(?i)no bonded hosts|bond.*(lost|gone|missing)", timeout=20.0,
                   what="blooter to say at startup that it knows no hosts")


# --------------------------------------------------------------------------
# D7 -- a bond on the wrong transport
# --------------------------------------------------------------------------

@tests.test
def test_d7_transport_switch_invalidates_bonds(t):
    """Pair on Classic, then restart blooter with `protocol = "ble"`.

    The host holds a BR/EDR record (`0x1124`) and keeps dialing BR/EDR, which
    BlueZ fails with `br-connection-create-socket`, while blooter advertises
    HOGP beside it, unreachable. Changing `protocol` invalidates every existing
    bond (CONNECTION.md §8.1); the remedy is to re-pair on the new transport.

    This is one of the two rows that needs a real host kernel to mean anything:
    it is entirely about what the *host* cached, and only a real host caches.
    """
    t.start_blooter(protocol="classic")
    t.pair_to_working_input()
    bond = t.dev.bonded_to(t.host.address)
    assert bond and bond["linkkey"], f"the Classic bond is not a link key: {bond}"

    t.dev.stop_blooter()
    t.start_blooter(protocol="ble")

    assert_broken(t)

    # The remedy: re-pair on the new transport, from the host.
    t.host.remove(t.dev.address)
    t.dev.remove_bond(t.host.address)
    t.pair_to_working_input()
    t.expect_working_input()

    bond = t.dev.bonded_to(t.host.address)
    assert bond and bond["ltk"], \
        f"the new bond is not an LE bond, so the transport did not really change: {bond}"


@tests.test(xfail="CONNECTION.md §8.2 detection is not implemented yet")
def test_d7_detection(t):
    """§8.2.1's headline check: *every bonded host's transport matching the
    configured protocol*. It is entirely local -- blooter knows the configured
    protocol and it knows what kind of key each bond holds -- so it must be said
    at startup, before a host that can never connect is waited for."""
    t.start_blooter(protocol="classic")
    t.pair_to_working_input()
    t.dev.stop_blooter()
    t.start_blooter(protocol="ble")

    t.dev.wait_log(rf"(?i){t.host.address}.*(classic|br/edr|transport|carry over)",
                   timeout=20.0,
                   what="blooter to warn that the existing bond is on the other "
                        "transport")


# --------------------------------------------------------------------------
# D8 -- the host's cached descriptor goes stale
# --------------------------------------------------------------------------

GAMEPAD_OFF = '[gamepad]\nslots = 0\n'
GAMEPAD_ON = '[gamepad]\nslots = 1\n'


def _blooter_inputs(t):
    return [n for n in t.host.input_devices() if n.startswith("blooter ")]


def _start_over_uinput(t, protocol, config_extra):
    devices = t.dev.make_uinput([KEY_A], [], MOUSE_RELS)
    return t.start_blooter(
        protocol=protocol, fifo=False, interactive=True,
        config_extra=config_extra,
        extra_args=["-e", str(devices["keyboard"]), "-e", str(devices["mouse"])])


@tests.test
def test_d8_stale_descriptor_classic(t):
    """`[gamepad] slots` changes between runs; the host keeps the descriptor it
    cached and the new gamepad never appears, silently.

    The fix on Classic is the virtual-cable unplug behind `[f]` (§7.2a), and
    the assertion is made by the *host*: it re-reads the SDP record on the next
    pairing and the device appears. The other row that needs a real host kernel.
    """
    _start_over_uinput(t, "classic", GAMEPAD_OFF)
    t.pair_to_working_input()
    before = _blooter_inputs(t)

    # A different descriptor, same bond.
    t.dev.stop_blooter()
    _start_over_uinput(t, "classic", GAMEPAD_ON)
    t.reconnect_host()

    # -- the symptom: nothing changes, and nothing says so --
    t.attach_input()
    menus = t.dev.count(r"\[f\] Fix connection")
    assert _blooter_inputs(t) == before, (
        "the host picked the new descriptor up on its own, so this row is no "
        f"longer testing anything: {before} -> {_blooter_inputs(t)}")

    # -- the remedy: [f], then re-pair, exactly as blooter instructs --
    #
    # The host has to be *away* first. An incoming connection preempts the menu
    # and blooter takes it as the user's intent (CONNECTION.md §6.2), so a `[f]`
    # pressed during a session is discarded -- which is what happened when this
    # test was first written, and is worth knowing before writing another one.
    # The host still has to be reachable, since the fix dials its control PSM.
    t.host.disconnect(t.dev.address)
    # Wait for the list to be *drawn*, not merely for the menu to start: the
    # Classic picker opens with a 4 s discovery scan, and a key pressed while
    # that scan is running goes nowhere.
    t.dev.wait_log(r"\[f\] Fix connection", timeout=30.0,
                   what="the menu to re-open and list the host", after=menus)

    t.dev.menu("f")
    wait_for(lambda: not t.dev.bonded_to(t.host.address), 45.0,
             "the unplug to drop both halves of the bond")

    t.host.remove(t.dev.address)
    t.pair_to_working_input()
    after = _blooter_inputs(t)
    assert len(after) > len(before), (
        f"the host re-paired but still shows the old layout: {before} -> {after}")
    t.expect_working_input()


@tests.test
def test_d8_stale_descriptor_ble_advice(t):
    """`[f]` on BLE with the host away must say so and change nothing.

    §7.2b, step 2, and one of the places the project already got this wrong
    once: a failed fix used to drop the bond, which for a host that never
    advertises means it can never be rediscovered. **A failed operation must not
    drop a bond.** So the assertion is as much about what did *not* happen.
    """
    _start_over_uinput(t, "ble", GAMEPAD_OFF)
    t.pair_to_working_input()
    t.host.disconnect(t.dev.address)
    t.dev.wait_log(t.dev.DISCONNECTED, timeout=30.0,
                   what="the host to go away before [f] is pressed")

    t.dev.menu("f")
    t.dev.wait_log(r"(?i)not connected|connect from", timeout=20.0,
                   what="blooter to say the host has to connect first")
    assert t.dev.bonded_to(t.host.address), \
        "a [f] that could not be performed dropped the bond anyway"

    # And the advice is honest: doing what it says gets a working session back.
    t.reconnect_host()
    t.expect_working_input()


@tests.test(xfail="[f] needs a connected host, but the menu is only open while "
                  "no host is connected (CONNECTION.md §6.2 vs §7.2b)")
def test_d8_stale_descriptor_ble_reread(t):
    """The BLE half of D8: Service Changed makes the host re-read the Report
    Map, with no re-pairing anywhere.

    Written now and allowed to fail (§6.1). `Le::fix_host` needs
    `Device.Connected`, and `wait_connected` cancels and joins the menu the
    moment a host connects -- so the state in which `[f]` does anything is not
    obviously reachable from a real session. If this starts passing, the marker
    comes off; if it keeps failing, that is the finding.
    """
    _start_over_uinput(t, "ble", GAMEPAD_OFF)
    t.pair_to_working_input()
    before = _blooter_inputs(t)

    t.dev.stop_blooter()
    _start_over_uinput(t, "ble", GAMEPAD_ON)
    t.reconnect_host()

    t.dev.menu("f")
    t.dev.wait_log(r"(?i)Service Changed|cached copy", timeout=20.0,
                   what="blooter to indicate Service Changed")

    wait_for(lambda: len(_blooter_inputs(t)) > len(before), 60.0,
             "the host to re-read the Report Map and build the new device")
    assert t.dev.bonded_to(t.host.address), "the BLE fix must touch no bond"


# --------------------------------------------------------------------------
# D9 -- the host forgets blooter mid-session
# --------------------------------------------------------------------------

@both_transports
def test_d9_remove_during_session(t, protocol):
    """`remove` while the link is live: it drops, blooter goes back to
    accepting rather than dying, and a re-pair restores input."""
    t.start_blooter(protocol=protocol)
    t.pair_to_working_input()

    t.host.remove(t.dev.address)
    t.dev.wait_log(t.dev.DISCONNECTED, timeout=45.0,
                   what="the live link to drop when the host forgot us")
    assert t.dev.alive(), "blooter exited when the host removed it mid-session"

    # Back to accepting: whatever blooter's half is now, a re-pair works.
    if t.dev.bonded_to(t.host.address):
        t.dev.remove_bond(t.host.address)
    t.pair_to_working_input()
    t.expect_working_input()
