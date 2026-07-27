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

- **Reconnect-initiate / the outgoing dial** (design/CONNECTION.md §3.2, and its
  BLE counterpart in §4). No suite covers it over a real link: that needs blooter
  to initiate to an already-bonded target, which means driving a pairing first.
  The race, the exponential backoff and the one-shot target clearing are still
  only covered by reading.
- **Menu-driven pairing** (`finalize` in `menu.rs`). Picking an unbonded host from
  the menu drops raw mode, pairs from here, then initiates. `tests/termdbus`
  asserts that a BLE pick calls `Pair` then `Connect`, but the mock accepts
  `Pair` without driving the resulting agent exchange, and the Classic pick stops
  at the UI.
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
- **Bonded / encrypted BLE.** `tests/btvirt` covers the LE transport over a real
  link — advertising, the GATT tree, the CCCD-subscribe connect/disconnect
  semantics and report notifications — but subscribes on an *unencrypted* link,
  which blooter permits (only the Report reads and the Report Map are
  encryption-gated). A real HOGP host bonds first; those read paths are untested,
  blocked by the same shared-agent artifact as everything else that needs a
  pairing.
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

## To investigate (continued)

- **`SYN_DROPPED` is invisible through the evdev crate's synced stream.** When
  the kernel's evdev ring overflows it emits `SYN_DROPPED`, but
  `Device::into_event_stream()` never surfaces it: `sync_events`
  (evdev `sync_stream.rs`) discards the whole affected block and emits
  compensatory state-diff events instead, `block_dropped` is private and
  `EventStream` exposes no resync signal. Warning on a drop therefore needs
  `RawDevice::into_event_stream()`, which yields it verbatim — every method
  `input.rs` uses (`supported_keys`, `supported_events`, `get_absinfo`, `name`,
  `input_id`, `grab`, `ungrab`, `send_events`) exists on `RawDevice`, and only
  `get_absinfo`'s return shape differs. blooter tracks its own key state in
  `InputState`, so evdev's state machine is not load-bearing. Doing this would
  also fix a latent bug: after a resync the compensatory `ABS_X`/`ABS_Y` event
  is differenced by `translate_abs` against a stale `last_abs`, producing a
  clamped pointer jump where the reference should just be reseeded (§7.2b).
  Batching (§7.2c) makes drops much less likely but no more visible.
- **`Shared::notify` allocates and locks per report** (`transport/le.rs`). Each
  notification takes an async mutex on the notifier map and does a
  `payload.to_vec()` — on the hot path AGENTS.md calls out. Batching cuts how
  often this runs, but the allocation should go: reuse a per-connection buffer
  and copy into it, or hold the notifier without the map lookup.
- **BLE has no "fix connection" for a descriptor change.** The GATT tree
  (`hid_service`, `transport/le.rs`) declares no Service Changed characteristic
  (`0x2A05`), and `Le` never touches `state::Hosts`. Hosts cache the Report Map
  across a bond exactly as Classic hosts cache the SDP record, so changing
  `[gamepad] slots` or `[pointer] axis_bits` leaves a bonded BLE host misreading
  reports with no way to repair it short of re-pairing by hand. Since BLE is the
  default transport this is the bigger of the two caching gaps.

## Done since (kept here for history)

- **Running the suites in CI** — done: `.github/workflows/ci.yml` runs unit,
  fmt/clippy, `btvirt` and `termdbus` on GitHub-hosted `ubuntu-24.04`. KVM is
  available on the standard runners once a udev rule makes `/dev/kvm`
  world-writable, and Ubuntu packages everything else at the paths the
  harnesses hardcode. The runner's own kernel (`linux-azure`) has no `hci_vhci`,
  so CI installs `linux-image-generic` and points the VM at it through the
  `VNG_KERNEL` override both `run.sh` scripts now accept; the `btvirt` job
  preflights `/dev/vhci` in the guest before running anything.

- **Pairing / agent handling** — implemented: a shared BlueZ agent is registered
  for both transports, auto-accepting ("Just Works") or prompting on the TTY per
  `[connection] pairing` (design/CONNECTION.md §5).
- **Outgoing HID (reconnect-initiate) connections** — implemented: when a
  reconnect target is set (the host menu, or `[connection] reconnect`), the
  Classic transport dials the host's HID L2CAP PSMs, raced against the inbound
  accept (design/CONNECTION.md §3.2, §6). BLE does the same with
  `Device::connect()`, raced against the CCCD subscribe (§4).
- **BLE as the default transport, with its own connection menu** — implemented:
  `[connection] protocol` defaults to `"ble"`, and both transports run the same
  `menu.rs` (differing only in the discovery filter and whether `[f]` applies).
  Covered by `tests/termdbus` (the menu) and `tests/btvirt` (the link).
- **Batching pointer events for slower connections** — implemented: pointer
  motion accumulates per input frame and is flushed under a configurable policy
  (`[pointer] batch`/`buffer`/`axis_bits`/`overflow`, design/ARCH.md §7.2c). This
  also fixed `translate_rel`/`translate_abs` emitting one report *per axis*, and
  added an optional 16-bit axis descriptor variant (§3.2) so merged motion never
  saturates.

- **Absolute-pointer / touchpad support** — implemented: touchpad `EV_ABS`
  positions are converted to relative mouse motion (design/ARCH.md §7.2b).
- **Gamepads** — implemented: one or more USB gamepads are forwarded as HID game
  controllers, with runtime hotplug (design/ARCH.md §5, §6.4, §7.5).
