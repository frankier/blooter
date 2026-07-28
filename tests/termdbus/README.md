# Menu and pairing-prompt tests

Drives blooter's interactive TUI under a real PTY, against a scripted BlueZ.
The counterpart to `tests/btvirt`: that suite brings up a real L2CAP link but
must run blooter non-interactively; this one inverts the trade, mocking the
Bluetooth stack so the terminal behaviour can be driven exactly.

```
  python-dbusmock (org.bluez)  ←─ D-Bus ─→  blooter  ─ PTY →  termwright
    scripted devices,                                         screen text,
    scripted agent calls                                      keystrokes
```

**Real here:** blooter itself, its crossterm rendering, its raw-mode handling,
and the `TermCoord` hand-off between the menu and the pairing prompt.
**Mocked:** everything below D-Bus. No controller is involved.

## Running

```sh
./run.sh              # whole suite
./run.sh pairing      # only tests whose name contains "pairing"
```

Prerequisites:

```sh
pip install --user python-dbusmock      # the mocked org.bluez
cargo install termwright                # PTY automation
sudo dnf install virtme-ng fontconfig-devel
```

`fontconfig-devel` is only needed to *build* termwright — it pulls in `font-kit`
for a PNG-screenshot feature these tests never use. `virtme-ng` is the only
other root-requiring install; the VM itself runs as your user.

## Why a VM, when BlueZ is mocked

Because blooter binds L2CAP PSMs 0x11 and 0x13 at startup, below 0x1001, which
needs `CAP_NET_BIND_SERVICE`. Verified: that bind fails for a normal user *and*
under `unshare -Ur`, since the capability is checked against the initial user
namespace. Inside the guest we are genuinely root.

No controller is needed for the bind to succeed, so unlike `tests/btvirt` this
harness starts **no btvirt and no bluetoothd** — the mock is the whole stack.

## What the tests cover

- **Listing** — address, alias, and the `[unpaired]`/`[paired]` markers.
- **The "Other devices" split** (§6) — audio-class devices and devices with no
  `Name` property are filed out of the main host list.
- **Navigation** — arrow keys move the cursor and clamp at both ends; `[o]`/`[b]`
  enter and leave the submenu; `[r]` rescans; `[q]` skips.
- **The pairing prompt and terminal hand-off** (§5.2) — the interesting one. An
  inbound `RequestConfirmation` arriving *while the menu is live in raw mode*
  must suspend the menu, print the prompt, and read the reply from the same
  stdin. If the menu's `EventStream` swallowed the keystroke instead, pairing
  would stall forever. Also covers rejecting with `n`, and that the menu resumes
  and still responds to keys afterwards.
- **The BLE menu** (§4, §6) — the same menu under `protocol = "ble"`: that it
  renders at all, that the "Other devices" split falls back to GAP Appearance
  when a device has no Class of Device (as an LE-only peer does not), that `[f]`
  is *not* offered on BLE, and that picking an unbonded host calls `Pair` and
  then `Connect`.

## Things worth knowing before editing

- **Read the *last* menu block, not the screen.** The menu repaints by moving up
  and clearing, but near the bottom of the terminal the scroll leaves earlier
  renders visible above the current one. `last_menu_block()`, `menu_title()`,
  `selected_index()` and `assert_menu_contains()` all scope to the current
  render; a plain `assert_screen_contains` can be satisfied by a stale one. Use
  `wait_for_menu(term, title)` rather than `wait_for_text(title)` when switching
  submenus, for the same reason.
- **Device order is not insertion order.** blooter lists devices in whatever
  order D-Bus enumeration yields, so tests assert on row *numbers*, never on
  which device happens to be first.
- **Never introspect our own bus name.** `_agent_owner` has to find which
  connection exports blooter's agent, and a synchronous call to our own
  connection deadlocks until the D-Bus timeout. Our name and the mock's are
  skipped, and every other probe has a short timeout.
- **Close `AgentCall` connections.** Each agent call runs on its own thread with
  its own bus connection; leaving them open strands idle names that the next
  owner scan pays a probe timeout for.
- **`named=False` deletes the `Name` property** rather than blanking it, because
  blooter keys off the property being absent — an empty string still counts as a
  real name. dbusmock has no D-Bus call for deleting a property, so the harness
  uses `AddMethod` to run the deletion inside the mock process. `cls=None` does
  the same to `Class`, which is how an LE-only peer looks.
- **The transport is always pinned** with a written config file passed as `-c`,
  never left to blooter's default. A change of default must not silently move a
  test from one transport to the other.
- **dbusmock's bluez5 template has no `GattManager1`**, which the LE transport
  needs to register its HOGP tree. `add_adapter` stubs it; the BLE link itself is
  `tests/btvirt`, so accepting the registration is all that is needed here.

## What this suite does not cover

- **The actual link** — connecting, report forwarding, disconnect/reconnect.
  That is `tests/btvirt`.
- **Pairing from the menu over a real link** — the BLE tests assert that a pick
  calls `Pair` and then `Connect`, but the mock does not drive the resulting
  agent exchange, so the outgoing-pair path is not exercised end to end. The
  Classic pick is not covered at all.
- **`[f] Fix connection`** — its presence in the footer is asserted on both
  transports, but not what it performs (the Classic unplug-and-unbond, the BLE
  connect-and-churn), which needs a real link.
- **The BLE link itself** — advertising, the CCCD subscribe and report
  notifications are `tests/btvirt`; here the LE transport's GATT registration is
  a mock stub.
