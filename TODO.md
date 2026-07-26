# TODO / out of scope

Behaviour that blooter deliberately does **not** implement (yet), what the test
suites do **not** cover, and open questions worth a look. Some of the non-goals
have since been done and are noted as such.

## Not implemented

- **Output reports and control-channel features.** No keyboard-LED (output
  report) handling, no boot-protocol mode switching, and no real
  `GET_REPORT`/`SET_REPORT`/`GET_PROTOCOL`/`SET_PROTOCOL` support — such
  requests get a `HANDSHAKE ERR_UNSUPPORTED_REQUEST` reply (design/ARCH.md §4).
- **Multiple adapters / multiple concurrent hosts.** A single adapter and a
  single connected host at a time.

## Test coverage gaps

The two integration suites (tests/README.md) leave these paths unexercised.
Roughly in order of how much risk they carry.

- **Reconnect-initiate / the outgoing dial** (design/CONNECTION.md §3.2). Neither
  suite covers it: it needs blooter to dial an already-bonded target, which means
  driving a pairing first. Arguably the largest untested block of connection
  logic — the race, the exponential backoff, and the one-shot target clearing are
  all only covered by reading.
- **Menu-driven pairing** (`finalize` in `menu.rs`). Picking an unbonded host from
  the menu drops raw mode, pairs from here, then dials. `tests/termdbus` asserts
  the pick UI but stops before the pair; the mock accepts `Pair` without driving
  the resulting agent exchange.
- **`[f] Fix connection`** (§7). Only its presence in the footer is asserted. The
  unplug + unbond it performs needs a real link, so it belongs in
  `tests/btvirt` — but it also needs a bonded peer that can be re-paired
  afterwards, which is the shared-agent problem below.
- **First-contact pairing over a real link.** `tests/btvirt` bonds the two
  controllers *before* blooter starts, to dodge the shared-agent artifact, so
  every test there exercises the already-bonded reconnect path. Pairing a
  genuinely new host is untested at the link level.
- **Reconnecting after a virtual-cable unplug.** blooter correctly drops its bond
  on unplug, so reconnecting needs a fresh pairing — same blocker.
- **BLE / HOGP** (§4). Entirely untested: no suite touches the LE transport,
  advertising, or the CCCD-subscribe connect/disconnect semantics.
- **Output-report and control-channel handling.** Only the
  `ERR_UNSUPPORTED_REQUEST` reply is asserted; if the features above are ever
  implemented they arrive with no test scaffolding.

## To investigate

- **Menu repaints may stack at the bottom of the terminal.** Under
  `tests/termdbus` (100x30 PTY) each menu redraw left the previous render
  visible above it, rather than overwriting in place — several stacked copies
  after a few keypresses. `draw_lines` (`menu.rs`) does
  `MoveToPreviousLine(prev)` + `Clear(FromCursorDown)`, which is correct when the
  block does not straddle a scroll; the suspicion is that once the block sits at
  the bottom and printing scrolls the screen, the cursor row is clamped and the
  move-up lands in the wrong place. **Not confirmed on a real terminal** — it may
  be a vt100-emulator artifact. Worth reproducing by hand in a short window
  (`stty rows 12`) before changing anything. The harness works around it by only
  ever reading the last menu block.
- **Hiding the host-side controller from `bluetoothd`.** The root cause of the
  pre-bonding workaround in `tests/btvirt`: both link ends live on one
  `bluetoothd`, so once blooter registers its agent that single agent is the
  default for *both* adapters, and an unbonded connect raises simultaneous SSP
  requests — the second gets "Device or resource busy" and the connect is refused
  with `EACCES`. If hci1 could be kept out of bluetoothd's view (or given its own
  agent), most of the gaps above open up at once. No obvious mechanism found:
  `bluetoothd` has no adapter filter, and mgmt has no "ignore this controller".
- **RootCanal as a btvirt replacement.** btvirt turned out to carry BR/EDR L2CAP
  on both HID PSMs fine (`tests/btvirt` runs on it), so this is not currently
  needed — but if
  a scenario does hit its limits, Android's RootCanal has a far more complete
  controller model built for multi-device emulation. It speaks HCI over TCP, so
  it would still need the VM to bridge into `/dev/vhci`.
- **Running the suites in CI.** Both need KVM and a VM. `tests/btvirt` also
  builds btvirt from bluez source. Neither is impossible on a self-hosted runner
  with `/dev/kvm`, but nothing has been tried.

## Done since (kept here for history)

- **Pairing / agent handling** — implemented: a shared BlueZ agent is registered
  for both transports, auto-accepting ("Just Works") or prompting on the TTY per
  `[connection] pairing` (design/CONNECTION.md §5).
- **Outgoing HID (reconnect-initiate) connections** — implemented: when a
  reconnect target is set (the host menu, or `[connection] reconnect`), the
  Classic transport dials the host's HID L2CAP PSMs, raced against the inbound
  accept (design/CONNECTION.md §3.2, §6).
- **Absolute-pointer / touchpad support** — implemented: touchpad `EV_ABS`
  positions are converted to relative mouse motion (design/ARCH.md §7.2b).
- **Gamepads** — implemented: one or more USB gamepads are forwarded as HID game
  controllers, with runtime hotplug (design/ARCH.md §5, §6.4, §7.5).
