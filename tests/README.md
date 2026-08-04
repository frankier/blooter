# blooter tests

```sh
./run-all.sh              # unit tests, then all three integration suites
./run-all.sh btvirt       # one suite only (unit | btvirt | termdbus | twovm)
```

Each suite runs even if an earlier one fails, so a single invocation reports the
whole picture; the exit status is non-zero if any suite failed.

| Suite | What it proves | Stack |
|---|---|---|
| `unit` | report encoding, keymap, config, menu rendering | `cargo test` |
| [`btvirt`](btvirt/) | the real HID link on both transports: connect, forward reports, disconnect, unplug | real bluetoothd + emulated controllers, real L2CAP and ATT/GATT |
| [`termdbus`](termdbus/) | the interactive menu (Classic and BLE) and the pairing prompt | mocked BlueZ + real PTY |
| [`twovm`](twovm/) | bonding, and what happens when the two halves of a bond disagree | two VMs, two bluetoothds, two bond stores, one emulated radio |

The three integration suites are deliberately complementary:

| Suite | Bond | Link | Host stack |
|---|---|---|---|
| `btvirt` | pre-bonded via `btmgmt`, never negotiated | real L2CAP / real ATT | fake (raw sockets, `btgatt-client`) |
| `termdbus` | mocked | none | mocked |
| `twovm` | negotiated, per-side, destroyable | real | real BlueZ + real kernel HID |

`btvirt` gets a genuine L2CAP link but has to bond the controllers before
blooter starts, so every one of its tests begins already bonded; `termdbus` gets
full control of the terminal and of what BlueZ reports, but no link at all.
`twovm` exists for the blind spot they share — with both ends of the link on one
`bluetoothd`, a bond cannot disagree with itself. Together they cover
design/CONNECTION.md, §8 included.

## Prerequisites

```sh
sudo dnf install virtme-ng fontconfig-devel glib2-devel readline-devel
pip install --user python-dbusmock
cargo install termwright
```

Only the `dnf` line needs root, and only once:

- `virtme-ng` — runs the integration suites in a VM. `twovm` boots **two** at
  once and needs `--network user` for them, so budget memory for both (1G each
  by default; set `VNG_MEMORY` to change it).
- `glib2-devel`, `readline-devel` — build `btvirt` (not packaged on Fedora),
  `btmgmt` (packaged, but older ones hang when driven non-interactively),
  `btgatt-client` (the BLE tests' host side), and `btproxy`/`btmon` (the radio
  bridge and per-test HCI captures `twovm` uses).
- `fontconfig-devel` — builds `termwright` (for a screenshot feature the tests
  do not use).

Everything else installs into your home directory, and the VMs themselves run
unprivileged: `/dev/kvm` is world-writable and the host kernel is world-readable.

`.github/workflows/ci.yml` is the same thing for Ubuntu — it runs the suites on
a GitHub-hosted runner, so it doubles as the worked `apt` equivalent of the
`dnf` line above. Every VM boots the host kernel by default; set
`VNG_KERNEL=<version in /boot>` to boot another one, which is how CI works
around the runner kernel having no `hci_vhci`.

## Why the VMs

Two kernel facts, both confirmed on this machine:

- `AF_BLUETOOTH` sockets only work in the initial network namespace, so a
  container with its own netns cannot create one at all.
- blooter binds L2CAP PSMs 0x11/0x13 (below 0x1001), which needs
  `CAP_NET_BIND_SERVICE` — and that is checked against the initial user
  namespace, so `unshare -Ur` does not help either.

Inside the guest we are genuinely root, with our own `init_net` and `/dev/vhci`,
isolated from the real adapter. This is the same approach BlueZ upstream takes
with `tools/test-runner`.

`twovm` needs two of them for a third reason: the thing under test is a
disagreement between two independent BlueZ instances, and two `bluetoothd`
processes cannot share one kernel — it has no adapter-restriction flag, so both
would adopt both controllers.

Each suite's README documents its own stack, quirks, and coverage gaps.
