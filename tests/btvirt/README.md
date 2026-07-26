# End-to-end connection tests

Drives the real `blooter` binary over real L2CAP and asserts on the HID reports
that actually cross the interrupt channel. Nothing is mocked: blooter talks to a
real `bluetoothd` over a real system bus, on emulated controllers.

```
  btvirt hci0 <--- real L2CAP (PSM 0x11 / 0x13) ---> btvirt hci1
     |                                                  |
  bluetoothd                                        FakeHost
     |                                            (test process)
  blooter  <--- struct input_event via FIFO --- the test
```

## Running

```sh
./run.sh              # whole suite
./run.sh keyboard     # only tests whose name contains "keyboard"
```

Prerequisites (one command, the only step needing root):

```sh
sudo dnf install glib2-devel readline-devel virtme-ng
```

`glib2-devel` and `readline-devel` are needed to build `btvirt` and `btmgmt`
from bluez source (`build-btvirt.sh`), `virtme-ng` to run the VM. `btvirt` is
not packaged on Fedora at all (Debian has it in `bluez-test-tools`); `btmgmt` is
packaged everywhere but the harness drives it non-interactively, and versions
before ~5.8 never exit when driven that way — so both come from one pinned
source tree rather than from the distro. Everything else is unprivileged:
the build runs as you, the VM runs as you via KVM, and only *inside* the guest
are we root.

## Why a VM

Two kernel-level facts, both confirmed on this machine:

- `AF_BLUETOOTH` sockets only work in the initial network namespace —
  `bt_sock_create()` rejects any other netns outright, so `unshare -Urn` gives
  `EAFNOSUPPORT (97)` for HCI and L2CAP sockets alike. A container with its own
  netns cannot create one at all; `--network=host` works but then there is no
  isolation from the real `hci0` and the system `bluetoothd`.
- `/dev/vhci` is `crw------- root root`, and a user namespace does not help:
  the node's owner (real uid 0) is unmapped inside, so ns-root is not the
  owner. Rootless `podman --privileged --network=host` still gets `EACCES`.

blooter also needs to bind L2CAP PSMs 0x11/0x13, which are below 0x1001 and so
require `CAP_NET_BIND_SERVICE`. Inside the guest we are genuinely root, with our
own `init_net` and our own `/dev/vhci`, isolated from the real adapter.

## How the pieces fit

- **`run.sh`** — host side: builds blooter and btvirt, launches the VM.
- **`build-btvirt.sh`** — builds `emulator/btvirt` from bluez source into
  `build/`, no root and nothing installed system-wide. Everything bluez ships
  that the emulator does not need is `--disable`d, keeping the build under a
  minute; it is a no-op once the binary exists.
- **`guest/run-tests.sh`** — guest entry point, runs as root.
- **`guest/harness.py`** — the stack (`Stack`), the binary under test
  (`Blooter`), the fake host (`FakeHost`), and a minimal test runner (the guest
  has no pytest).
- **`guest/test_connection.py`** — the test cases.

**FIFO input mode is what makes blooter testable here.** `-f` bypasses evdev and
udev entirely, so a test writes `struct input_event` records straight into the
input pipeline and asserts on the HID reports that come out.

Component logs land in `/tmp/blooter-btvirt/` inside the guest and are dumped
automatically when a test fails.

## Two things the shared stack forces

Both ends of the link live on one `bluetoothd` here, which a real deployment
never does. That has two consequences worth knowing before editing the harness:

1. **The controllers are bonded up front**, before blooter starts. Once blooter
   registers its pairing agent, that one agent is the default for *both*
   adapters, so an unbonded connect raises simultaneous SSP requests on hci0 and
   hci1; the second gets "Device or resource busy", authentication fails, and
   the connect is refused with `EACCES`. Bonding first means connects need no
   SSP and never reach the agent. It is also the realistic state — a host that
   reconnects to blooter is one that already paired with it.

2. **Controller setup happens after `bluetoothd` starts**, because bluetoothd
   adopts the controllers and applies its own settings, clobbering anything set
   before it.

Two btvirt quirks also apply:

- **No sysfs `address`.** `-l` controllers have no
  `/sys/class/bluetooth/hciN/address` node; the harness reads the BD_ADDR from
  `btmgmt --index N info` instead.
- **Fixed addresses, `public-addr` ignored.** They come up as
  `00:AA:01:00:00:00` (hci0) and `00:AA:01:01:00:01` (hci1) and keep those even
  if you try to assign one. Read the real address; don't assign.

## What this suite does not cover

- **The pairing agent flow** (design/CONNECTION.md §5) — `auto` vs `confirm`,
  and the `TermCoord` dance where an inbound pairing prompt borrows the terminal
  from a running menu. The shared-agent artifact above makes agent-driven
  pairing untestable in this stack, and the terminal behaviour needs a PTY.
  That is the natural home for a **termwright + python-dbusmock** suite, which
  needs no VM at all.
- **The interactive menu** (§6) — same reason: it needs a PTY. Tests here run
  blooter with stdin on `/dev/null`, so it infers non-interactive and the menu
  stays out of the way.
- **Reconnect-initiate / the outgoing dial** (§3.2) — needs blooter to dial a
  bonded target, which means driving pairing first.
- **Reconnecting after a virtual-cable unplug** — blooter correctly drops its
  bond on unplug, and re-pairing hits the shared-agent artifact.
- **BLE / HOGP** (§4) — a separate transport; nothing here touches it.

## Adding a test

```python
@tests.test
def test_something(t):
    blooter = t.start_blooter()      # fresh blooter, FIFO input
    host = t.connected_host()        # fake host, both PSMs, connection confirmed
    host.drain()                     # ignore anything already queued

    blooter.key(KEY_A, True)
    assert_report(host.recv_report(), keyboard_report(keys=[4]), "'a' pressed")
```

`t.connected_host()` counts the connection lines blooter has already logged and
waits for a *new* one, so it works for reconnects too. Use `assert_report`
rather than a bare `==` — it prints both reports as hex and points at the
differing bytes.
