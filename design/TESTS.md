# Two-host end-to-end tests

A third integration suite, in which blooter and the host it talks to are **two
separate machines** — separate kernels, separate `bluetoothd`, separate pairing
agents, separate bond storage — joined by an emulated radio. It exists to test
the one thing neither existing suite can reach: **bonding, and what happens when
the two halves of a bond stop agreeing.**

Everything in §2 was verified before this document was written; §2.5 records the
evidence. Nothing here is assumed to work.

## 1. Why a third suite

`tests/README.md` presents `btvirt` and `termdbus` as complementary, and they
are, but they share a blind spot. From `tests/btvirt/README.md`, its own list of
gaps: the pairing agent flow, reconnect-initiate, re-pairing after an unplug, and
bonded/encrypted BLE. All four are blocked by one root cause — **both ends of the
link live on one `bluetoothd`**, so a single agent is the default for both
adapters and an unbonded connect raises simultaneous requests on both. The
harness works around it by bonding the controllers with `btmgmt` *before* blooter
starts, which means every `btvirt` test begins from an already-bonded state.

So the bond is exactly what is never tested. That is not a hypothetical gap:

- The `AuthenticationFailed` bug (CONNECTION.md §5) lived entirely inside the
  agent callback that `btvirt` cannot reach. It shipped, and it made BLE unusable
  from a cold start, while all 18 `btvirt` tests passed.
- Every state in CONNECTION.md §8.1 — half bonds, wrong-transport bonds — is by
  definition a *disagreement between two bond stores*. One store cannot
  disagree with itself.

`termdbus` has full control of the terminal and of what BlueZ reports, but its
BlueZ is mocked and there is no link at all. It can test that the menu offers to
fix a host; it cannot test whether the fix works.

| Suite | Bond | Link | Host stack |
|---|---|---|---|
| `btvirt` | pre-bonded via `btmgmt`, never negotiated | real L2CAP / real ATT | fake (raw sockets, `btgatt-client`) |
| `termdbus` | mocked | none | mocked |
| **`twovm`** | **negotiated, per-side, destroyable** | **real** | **real BlueZ + real kernel HID** |

## 2. Architecture

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

- **`btvirt -t`** opens a TCP server of type `SERVER_TYPE_BREDRLE` — dual mode,
  so one server serves both transports. Each accepted client gets its own
  `btdev_create(BTDEV_TYPE_BREDRLE52, …)`, and `btdev.c` keeps every device in
  one process-global `btdev_list` that inquiry and LE scan walk. **Connecting two
  clients to one server is what makes them able to see each other**; there is no
  additional wiring.
- **`btproxy -c <addr> -p <port>`** bridges a TCP connection to that server onto
  a local `/dev/vhci` controller (`connect_tcp()` + `open_vhci()` in
  `tools/btproxy.c`). Inside each VM this appears as an ordinary `hci0`.
- **`vng --network user`** gives each guest slirp networking, where `10.0.2.2`
  reaches the host's loopback — which matters because `btvirt -t` binds
  `127.0.0.1`, not `0.0.0.0`.

### 2.1 Why the VMs still cannot talk to each other directly

Slirp puts each guest on its own isolated network. Both can reach the host;
neither can reach the other. All cross-VM coordination therefore goes **through
the host**, which is what `hub.py` is for (§8.2). This is a constraint to design
around, not a problem to solve — routing the control plane through the host also
keeps the test logic in one readable place.

### 2.2 Why VMs at all, still

Unchanged from `tests/btvirt/README.md`, and worth restating because it is the
reason this cannot be two containers: `AF_BLUETOOTH` sockets only work in the
initial network namespace, `/dev/vhci` is `root`-owned and a user namespace does
not help, and blooter binds L2CAP PSMs below `0x1001`. Inside each guest we are
genuinely root with our own `init_net` and our own `/dev/vhci`.

What is new is that we now need **two** of them, because the thing under test is
a disagreement between two independent BlueZ instances. Two `bluetoothd`
processes on one kernel is not an option: `bluetoothd` has no adapter-restriction
flag (checked — `-i` does not exist), so both would adopt both controllers.

### 2.3 Addressing

`btvirt` assigns addresses by client index, deterministically: the first client
to connect gets `00:AA:01:01:00:43`, the second `00:AA:01:02:00:43`. **Connection
order is therefore the identity of each VM** and the harness must serialise it —
`dev` connects, is confirmed up, then `host` connects. As in the existing suite,
addresses are read back (`btmgmt --index 0 info`), never assigned; btvirt ignores
`public-addr`.

### 2.4 Privilege

The host side is entirely unprivileged: `btvirt -t` needs no `/dev/vhci` (it is
the server, not a controller), `vng` runs via `/dev/kvm`, and only *inside* the
guests are we root. The one-time `dnf`/`apt` line from `tests/README.md` is
unchanged; `btproxy` is added to the existing build.

### 2.5 Evidence

Each claim above was checked on this machine before being written down:

| Claim | How it was checked |
|---|---|
| `btproxy` builds from the vendored tree | `make -C build/bluez-5.87 tools/btproxy` — builds in seconds, no configure change |
| `btvirt -t` is dual-mode, binds loopback | source (`server_open_tcp(SERVER_TYPE_BREDRLE, …)`); runtime: `Listening TCP on 127.0.0.1:45550` |
| a client of the server appears as a real controller in a guest | one `vng --network user` VM, `btproxy -c 10.0.2.2 -p 45550` → `hci0` present in `/sys/class/bluetooth` |
| **two guests on separate kernels see each other** | VM A advertising at `00:AA:01:01:00:43`; VM B (separate VM) `btmgmt find -l` → `dev_found: 00:AA:01:01:00:43 type LE Public` |

## 3. Roles

- **`dev`** — runs blooter, exactly as a user would: real config file, real
  transport, real agent. Connects to the radio first (§2.3).
- **`host`** — runs stock BlueZ with an auto-accepting pairing agent, and is
  otherwise an ordinary Linux Bluetooth host. It initiates pairing, and its
  kernel drives the HID device that results.

The asymmetry is deliberate. `host` is never taught anything about blooter; if a
test passes, it passes against a stock BlueZ host doing ordinary things.

## 4. What "working keyboard/mouse" means

The existing suite asserts on HID report bytes crossing the link. Here the
assertion is a full round trip, because the host has a real kernel HID stack:

1. inject a `struct input_event` on `dev` (FIFO mode, §4.1),
2. blooter encodes it and sends it over the real link,
3. the **host kernel's** HID parser consumes the Report Map, creates input
   devices, and emits evdev events,
4. read those events back on `host` and compare.

The intermediate observable — the host creating the devices at all — is itself an
assertion worth making, and is what the report descriptor is really being tested
against:

```
input: blooter Keyboard as /devices/virtual/misc/uhid/…/input/inputN
input: blooter Mouse    as /devices/virtual/misc/uhid/…/input/inputM
hid-generic …: BLUETOOTH HID v… Mouse [blooter] on <dev addr>
```

(That is the real output observed from a hardware run; the suite asserts the same
shape.) The host finds the nodes by matching `/sys/class/input/event*/device/name`
against `blooter Keyboard` / `blooter Mouse` rather than guessing numbers.

**This is the strongest statement the project can make**: not "the right bytes
went out" but "a stock Linux host ended up with a working keyboard and mouse".

### 4.1 Two input paths

- **FIFO (`-f`)** for most tests: deterministic, no devices to fabricate, and the
  path `btvirt` already uses. Note it disables gamepad forwarding and never
  exercises `EVIOCGRAB`.
- **uinput** for the few that need the real thing: create a virtual keyboard and
  mouse in the `dev` VM, run blooter with `-e`/`-x` against them, and inject
  through uinput. This is the only way to cover the evdev path, the exclusive
  grab, and capture toggling — all of which are invisible to FIFO mode, and one
  of which (`-x` capture) is where a user-reported symptom landed.

## 5. Core journeys

Run for **both** `protocol = "ble"` and `protocol = "classic"`. Every one starts
from genuinely unpaired adapters — no `btmgmt pair` preamble.

| # | Journey | Asserts |
|---|---|---|
| J1 | cold pair → working input | unpaired → host initiates → bond on both sides → HID devices appear on host → injected keys/motion arrive |
| J2 | disconnect → reconnect | host drops the link and reconnects with no re-pairing; input works again |
| J3 | blooter restart | `dev` restarts blooter; the host reconnects (BLE: to the advertisement; Classic: dial or accept) |
| J4 | host reboot | bond survives on both sides; input works after the host comes back |
| J5 | capture toggle (uinput) | `-x` grabs on connect, releases on capture-off, re-grabs on capture-on |

J1 is the one that would have caught the `AuthenticationFailed` bug: it is
precisely "from unpaired adapters to a working keyboard and mouse", and it fails
loudly if the agent refuses its own pairing.

## 6. Divergence and recovery

The point of the suite. Each scenario **damages one or both bond stores out of
band** — directly via `bluetoothctl`/D-Bus on whichever side, never through
blooter's own menu — and then asks whether the user can get back to a working
keyboard.

Three things are asserted for every row, and the third is the one that matters:

1. **The symptom** — what actually breaks, so a regression that changes the
   failure mode is caught.
2. **Detection** — blooter notices and says which host and which problem
   (CONNECTION.md §8.2). Silence is a failure even if the link later recovers.
3. **The remedy works** — perform exactly the steps blooter printed, and end at a
   working keyboard and mouse. *This is the assertion that keeps blooter's advice
   honest*: a message telling the user to do something that does not fix it is a
   bug, and nothing else in the project can catch it.

| # | Out-of-band action | Side | Expected symptom | Remedy under test |
|---|---|---|---|---|
| D1 | `bluetoothctl remove <dev>` | host | host re-pairs from scratch; blooter holds a bond for a host that has none — BLE re-pair may fail against the stale key | blooter drops its half (menu `[u]`), host pairs again |
| D2 | `bluetoothctl remove <host>` | dev | host reconnects with a key blooter no longer has; link fails or drops at encryption | remove from the host's settings, pair again from the host |
| D3 | remove on both | both | none — clean slate | plain re-pair (control row: proves D1/D2 failures are the divergence, not the removal) |
| D4 | `trust off` | host | Classic: service authorization is refused unless blooter's agent answers; BLE: unaffected | reconnect must still work — this pins `AuthorizeService` |
| D5 | `trust off` | dev | reconnect still works (blooter's agent authorizes) | none needed; asserts we do not require trust |
| D6 | wipe `/var/lib/bluetooth` | dev | every host is unknown; all bonds one-sided | simulates a reinstall; each host must be re-pairable |
| D7 | pair on Classic, restart blooter with `protocol = "ble"` | dev | host holds a BR/EDR record (`0x1124`) and dials BR/EDR; BlueZ fails it with `br-connection-create-socket` while blooter advertises HOGP unreachably | re-pair on the new transport; **and** blooter must have said at startup that existing bonds do not carry over (CONNECTION.md §8.1) |
| D8 | change `[gamepad] slots` between runs | dev | host keeps the cached descriptor; the new gamepad never appears, silently | the §7 fix: BLE Service Changed (`[f]`), Classic virtual-cable unplug — asserted by the *host* re-reading the Report Map and the device appearing |
| D9 | `remove` mid-session | host | live link drops | blooter returns to accepting; re-pair restores input |

D7 and D8 are the two that need a real host kernel to mean anything: both are
about what the **host** cached, and only a real host caches.

Each row runs on both transports where meaningful. D4 is Classic-specific in
effect but is run on both to prove BLE is unaffected.

### 6.1 A note on what "detection" may assert today

CONNECTION.md §8.2 is a design commitment, not yet fully implemented. The
detection assertions should be written **now and allowed to fail**, marked
expected-fail, rather than omitted — they are the specification of §8, and an
expected-fail that starts passing is how the work gets recognised as done. The
symptom and remedy assertions in the same rows pass today and guard against
regression meanwhile.

## 7. Harness

### 7.1 Layout

```
tests/twovm/
  run.sh              host: build, start btvirt -t + hub.py, boot both VMs, report
  hub.py              host: TCP barrier + command fan-out; owns the test scripts
  guest/agent.py      in-VM: line-protocol agent, one per VM, dials the hub
  guest/dev.py        in-VM: bluetoothd + blooter under test (reuses btvirt's Stack)
  guest/host.py       in-VM: bluetoothd + auto-accept agent + evdev reader
  tests/test_*.py     host: the scenarios of §5 and §6
  README.md
```

### 7.2 Control flow

Tests are **host-side Python**, issuing commands to each VM through `hub.py`.
Because slirp lets each guest reach only the host, the hub is both the rendezvous
and the transport, and the test reads as one linear story:

```python
@tests.test
def test_ble_cold_pair_to_working_input(t):
    dev  = t.dev.start_blooter(protocol="ble")
    host = t.host

    host.pair(dev.address)                       # real SMP, real agent
    host.await_input_devices("blooter Keyboard", "blooter Mouse")

    dev.key(KEY_A, press=True)
    assert host.read_event() == (EV_KEY, KEY_A, 1)
```

The in-VM agents stay deliberately thin — start/stop daemons, run `bluetoothctl`,
open evdev nodes, write the FIFO — with no test logic, so a new scenario is a
host-side function and nothing else.

### 7.3 Reuse

`tests/btvirt/guest/harness.py` already has `Process`, `Stack` (dbus +
bluetoothd), `wait_for`, `input_event()` and a minimal runner, all of which apply
unchanged. Factor the shared parts into `tests/common/` rather than copying —
`Stack` in particular is the piece most likely to drift, and the two suites want
identical daemon startup.

### 7.4 Build

`build-btvirt.sh` gains `tools/btproxy` alongside `btvirt`/`btmgmt`/
`btgatt-client`; it is a single extra `make` target in the tree that is already
fetched and configured (§2.5). No new dependency, no configure change, and
`tests/README.md` gains a `twovm` row.

### 7.5 Failure diagnosis

Two VMs make a failure twice as hard to read, so the harness should, on failure,
dump: both blooter and `bluetoothd` logs, both sides' `bluetoothctl info` for the
peer, and — most valuable, from experience — an **`btmon` capture from each VM**,
kept per test. The `AuthenticationFailed` root cause was invisible in every log
and obvious in one line of `btmon`.

## 8. What this still cannot cover

Stated plainly, because the temptation with a suite this thorough is to assume it
covers everything:

- **Controller quirks and RF reality.** `btdev` is an idealised controller:
  perfect radio, no timing variation, no vendor firmware. A bug that only appears
  on an old Intel adapter cannot appear here. Hardware testing does not go away.
- **Non-BlueZ hosts.** Windows, macOS, Android and TVs are where HOGP
  interoperability actually gets interesting, and all of them are out of reach.
  Passing here means "correct against Linux", not "correct".
- **The terminal.** The menu and prompts remain `termdbus`'s job; these VMs run
  blooter non-interactively.
- **Suspend/resume and power management**, which is where real reconnect bugs
  often live.
- **Timing-dependent races** may reproduce differently or not at all, an emulated
  radio having none of the real one's jitter.

## 9. Implementation touch-points

- **`tests/twovm/run.sh`** — build blooter, `btvirt`, `btproxy`, `btmgmt`; start
  `btvirt -t` and `hub.py`; boot `dev` then `host` (order fixes addresses, §2.3);
  run the suite; tear down. Same `vng -r --user root` invocation as `btvirt`,
  plus `--network user`.
- **`tests/btvirt/build-btvirt.sh`** — add the `tools/btproxy` target (§7.4).
- **`tests/common/`** — `Process`, `Stack`, `wait_for`, `input_event`, the runner,
  moved out of `tests/btvirt/guest/harness.py` and imported by both suites.
- **`tests/README.md`** — a `twovm` row in the suite table, and the prerequisite
  note that it boots two VMs at once (memory, and `--network user`).
- **`design/CONNECTION.md` §8** — the detection assertions of §6.1 are that
  section's executable form; when §8.2 is implemented, the expected-fail markers
  come off.
