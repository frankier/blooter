# blooter tests

```sh
./run-all.sh              # unit tests, then both integration suites
./run-all.sh btvirt       # one suite only (unit | btvirt | termdbus)
```

Each suite runs even if an earlier one fails, so a single invocation reports the
whole picture; the exit status is non-zero if any suite failed.

| Suite | What it proves | Stack |
|---|---|---|
| `unit` | report encoding, keymap, config, menu rendering | `cargo test` |
| [`btvirt`](btvirt/) | the real HID link: accept, forward reports, disconnect, unplug | real bluetoothd + emulated controllers, real L2CAP |
| [`termdbus`](termdbus/) | the interactive menu and the pairing prompt | mocked BlueZ + real PTY |

The two integration suites are deliberately complementary. `btvirt` gets a
genuine L2CAP link but has to run blooter non-interactively; `termdbus` gets
full control of the terminal and of what BlueZ reports, but no link at all.
Together they cover both halves of design/CONNECTION.md.

## Prerequisites

```sh
sudo dnf install virtme-ng fontconfig-devel glib2-devel
pip install --user python-dbusmock
cargo install termwright
```

Only the `dnf` line needs root, and only once:

- `virtme-ng` — runs both integration suites in a VM.
- `glib2-devel` — builds `btvirt` (not packaged on Fedora).
- `fontconfig-devel` — builds `termwright` (for a screenshot feature the tests
  do not use).

Everything else installs into your home directory, and the VMs themselves run
unprivileged: `/dev/kvm` is world-writable and the host kernel is world-readable.

## Why both suites need a VM

Two kernel facts, both confirmed on this machine:

- `AF_BLUETOOTH` sockets only work in the initial network namespace, so a
  container with its own netns cannot create one at all.
- blooter binds L2CAP PSMs 0x11/0x13 (below 0x1001), which needs
  `CAP_NET_BIND_SERVICE` — and that is checked against the initial user
  namespace, so `unshare -Ur` does not help either.

Inside the guest we are genuinely root, with our own `init_net` and `/dev/vhci`,
isolated from the real adapter. This is the same approach BlueZ upstream takes
with `tools/test-runner`.

Each suite's README documents its own stack, quirks, and coverage gaps.
