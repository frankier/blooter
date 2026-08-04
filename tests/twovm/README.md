# Two-host end-to-end tests

blooter and the host it talks to run on **two separate machines** — separate
kernels, separate `bluetoothd`, separate pairing agents, separate bond storage —
joined by an emulated radio. That is what makes the one thing neither other
suite can reach testable: **bonding, and what happens when the two halves of a
bond stop agreeing.**

The design is design/TESTS.md; this file is what you need to run it, read a
failure, and add a row.

```
                    host machine (unprivileged)
        ┌──────────────────────────────────────────────┐
        │  btvirt -t          the emulated radio       │
        │  127.0.0.1:45550    one btdev per client,    │
        │       ▲   ▲         all in one btdev_list    │
        │       │   │                                  │
        │  hub.py (barrier + command fan-out, TCP)     │
        └───────┼───┼──────────────────────────────────┘
         10.0.2.2   10.0.2.2      (slirp: guest → host loopback)
    ┌───────────┴──┐  ┌─┴────────────┐
    │  VM "dev"    │  │  VM "host"   │
    │  btproxy -c  │  │  btproxy -c  │
    │    → hci0    │  │    → hci0    │
    │  bluetoothd  │  │  bluetoothd  │
    │  blooter     │  │  agent +     │
    │              │  │  kernel HID  │
    └──────────────┘  └──────────────┘
```

`btvirt -t` is a dual-mode TCP server; every client that connects becomes a
`btdev` in one process-global list that inquiry and LE scan walk, so **connecting
two clients to one server is the whole of the wiring**. `btproxy -c` bridges each
guest's end of that connection onto its own `/dev/vhci`, where it appears as an
ordinary `hci0`.

## Running

```sh
./run.sh                # whole suite
./run.sh cold_pair      # only tests whose name contains "cold_pair"
```

Prerequisites are the same as `tests/btvirt` (`virtme-ng`, `glib2-devel`,
`readline-devel`), with one extra consideration: this suite boots **two** VMs at
once. Budget memory for both — 1G each by default, `VNG_MEMORY` to change it.
`btproxy` and `btmon` come from the same bluez tree `tests/btvirt` already
builds; no new dependency, no configure change.

## What "working keyboard/mouse" means here

`tests/btvirt` asserts on the HID report bytes crossing the link. Here the
assertion is a full round trip, because the host has a real kernel HID stack:

1. an `input_event` is injected on `dev`,
2. blooter encodes it and sends it over the real link,
3. the **host kernel's** HID parser consumes the Report Map, creates input
   devices, and emits evdev events,
4. those events are read back on `host` and compared.

The intermediate observable — the host creating `blooter Keyboard` and
`blooter Mouse` at all — is itself an assertion, and is what the report
descriptor is really being tested against. The nodes are found by matching
`/sys/class/input/event*/device/name`, never by guessing numbers.

**This is the strongest statement the project can make**: not "the right bytes
went out" but "a stock Linux host ended up with a working keyboard and mouse".

## Layout

```
tests/twovm/
  run.sh                 host: build, then hand over to hub.py
  hub.py                 host: btvirt -t, both VMs, the TCP fan-out, the runner
  guest/agent.py         in-VM: line-protocol agent, one per VM, dials the hub
  guest/base.py          in-VM: btproxy → hci0 → dbus → bluetoothd, btmon
  guest/dev.py           in-VM: blooter under test (FIFO, uinput, and the menu)
  guest/host.py          in-VM: bluetoothctl + auto-accept agent + evdev reader
  tests/test_journeys.py    the core journeys (design/TESTS.md §5)
  tests/test_divergence.py  divergence and recovery (§6)
```

`Process`, `PtyProcess`, `wait_for`, `input_event` and the runner live in
`tests/common/` and are shared with `tests/btvirt` — `BluezStack` in particular,
because the two suites want *identical* daemon startup and it is the piece most
likely to drift.

## Control flow

Tests are host-side Python issuing commands to each VM through `hub.py`. Slirp
puts each guest on its own isolated network, so the VMs can reach the host but
never each other; routing everything through the hub is forced, and it also keeps
the test logic in one readable place:

```python
@both_transports
def test_cold_pair_to_working_input(t, protocol):
    t.start_blooter(protocol=protocol)
    t.pair_to_working_input()      # real SMP/SSP, real agents, real HID parse
    t.expect_key(KEY_A)            # injected on dev, read back on host
```

The in-VM agents are deliberately thin — start and stop daemons, run
`bluetoothctl`, open evdev nodes, write the FIFO — with **no test logic**, so a
new scenario is a host-side function and nothing else.

**Connection order is identity.** btvirt assigns addresses by client index, so
the hub tells `dev` to connect to the radio, confirms it is up, and only then
tells `host`. Addresses are read back from `btmgmt info`, never assigned.

## Two things worth knowing before editing

**blooter can be run with a terminal.** `interactive=True` puts blooter's stdin
and stdout on a pty, which is what lets a test press `[u]` or `[f]` and so assert
that the remedy blooter *printed* is one that actually works. This is not
terminal testing: `env_logger` writes to stderr, which stays on a plain file, so
every log assertion reads exactly the same bytes as in the non-interactive case,
and nothing asserts on rendering. The TUI itself remains `tests/termdbus`'s job.
Every scenario has exactly one host in the menu, so row 0 is already selected and
`[f]`/`[u]` need no navigation.

**Expected failures are part of the specification.** CONNECTION.md §8.2 is a
design commitment that is not yet fully implemented. Its detection assertions are
written here and marked `xfail` rather than omitted (design/TESTS.md §6.1) — they
are §8's executable form, and an xfail that starts passing is reported as `XPASS`
so the marker gets removed. The symptom and remedy assertions in the same rows
pass today and guard against regression meanwhile.

## Reading a failure

Two VMs make a failure twice as hard to read, so a failing test dumps, from both
guests at once: blooter's log, `bluetoothd` and the rest of the component logs,
each side's bond store, and a **per-test `btmon` capture**. That last one earns
its place — the `AuthenticationFailed` root cause was invisible in every log and
obvious in one line of `btmon`.

Everything also stays in `/tmp/blooter-twovm-{dev,host}/` inside each guest for
the life of the VM.

## What the first run turned up

Recorded because it is the point of the suite, and because two of these are
behaviours no other suite can see:

- **`[f]` and `[u]` need the host to be away.** An incoming connection preempts
  the menu and blooter takes it as the user's intent (CONNECTION.md §6.2), so a
  menu key pressed during a session is discarded. Any test of a menu remedy has
  to drop the link first — and on Classic the host must still be *reachable*,
  since `fix_host` dials its control PSM.
- **D1 does not diverge on Classic.** Removing the device on the host reaches
  blooter as a HIDP virtual-cable unplug and `run_session` drops our half to
  match (§7.2a), so no one-sided bond forms and a plain re-pair is the whole
  remedy. BLE has no unplug, and does diverge.
- **D2 self-heals on Classic.** With `pairing = "accept"` here and a
  `NoInputNoOutput` agent there, a reconnect against a bond only the host holds
  simply renegotiates a link key. The user sees nothing. On BLE the link dies at
  encryption instead, and the remedy has to be host-side.
- **The stale-layout warning works.** With `[gamepad] slots` changed under an
  existing bond, blooter names the host at startup and says to press `[f]` —
  the §7.1 detection, confirmed against a host that really did cache the old
  descriptor.
- **The Classic `[f]` unplug works; the host still shows the old layout.**
  Pressing `[f]` does drop both halves of the bond as §7.2a describes, and the
  re-pair that follows succeeds — but the host ends up with the same two input
  devices it had before, so the new gamepad slot never appears. Whether that is
  the cache not being refreshed or the gamepad collection simply not producing
  a separate input device is not yet established. The assertion is left failing
  rather than weakened, because it is the one D8 exists to make.
- **A BLE link that drops without a CCCD unsubscribe leaves the session open.**
  `test_disconnect_then_reconnect_ble` and the BLE `[u]` remedy both fail on
  this: after the host disconnects, blooter logs nothing, never returns to
  `wait_connected`, and so neither re-advertises nor re-opens the menu. This is
  a suspected blooter bug rather than a harness artifact — Classic recovers from
  the identical sequence — but it has not been root-caused, and the failing
  tests are left failing rather than adjusted to pass.

## Deliberate deviations from design/TESTS.md

- **J4 restarts the host's `bluetoothd`, not the host.** `vng` runs a guest for
  exactly one command, so a real reboot would end the VM. What J4 asks — does the
  bond survive on both sides, does input work when the host comes back — is
  fully exercised by restarting the daemon over its persisted bond storage, since
  that persisted state is the only thing a reboot would have carried across.
  Anything the *kernel* would forget is not covered.
- **D8 uses uinput rather than the FIFO.** `[gamepad] slots` has no effect in
  FIFO mode (`main.rs` forces the advertised count to zero there), so the one row
  that is about the descriptor changing has to come in over evdev.
- **D8's BLE re-read is `xfail`.** `Le::fix_host` requires `Device.Connected`,
  but `wait_connected` cancels and joins the menu the moment a host connects — so
  the state in which `[f]` does anything is not obviously reachable from a real
  session. The advice path (`[f]` with the host away says so and touches no bond)
  is asserted and passes.

## What this still cannot cover

Stated plainly, because the temptation with a suite this thorough is to assume it
covers everything:

- **Controller quirks and RF reality.** `btdev` is an idealised controller:
  perfect radio, no timing variation, no vendor firmware. A bug that only appears
  on an old Intel adapter cannot appear here.
- **Non-BlueZ hosts.** Windows, macOS, Android and TVs are where HOGP
  interoperability actually gets interesting. Passing here means "correct against
  Linux", not "correct".
- **The terminal.** The menu's rendering and the pairing prompt remain
  `tests/termdbus`'s job.
- **Suspend/resume and power management**, where real reconnect bugs often live.
- **Timing-dependent races**, which may reproduce differently or not at all — an
  emulated radio has none of the real one's jitter.

## Adding a test

```python
@both_transports                       # registers _classic and _ble variants
def test_something(t, protocol):
    t.start_blooter(protocol=protocol)
    t.pair_to_working_input()          # unpaired -> bonded -> HID devices open

    t.host.remove(t.dev.address)       # damage a bond store, out of band
    ...
    t.expect_working_input()           # and end at a working keyboard
```

Every test starts from genuinely unpaired adapters: the runner wipes both bond
stores before each one. There is no `btmgmt pair` preamble anywhere in this
suite — that preamble is exactly what `tests/btvirt` has to do, and exactly why
it can never test the bond.
