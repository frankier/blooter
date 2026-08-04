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

- **Reconnect-initiate / the outgoing dial** (design/CONNECTION.md §3.2), now
  Classic-only. No suite covers it over a real link: that needs blooter to
  initiate to an already-bonded target, which means driving a pairing first. The
  race, the exponential backoff and the one-shot target clearing are still only
  covered by reading.
- **Menu-driven pairing** (`finalize` in `menu.rs`), now Classic-only. Picking an
  unbonded host from the menu drops raw mode, pairs from here, then initiates.
  The Classic pick stops at the UI in `tests/termdbus`.
- **The `prompt` agent's passkey paths** (§5.2). `tests/termdbus` covers the
  y/n confirmation and the terminal hand-off around it; `RequestPasskey`,
  `DisplayPasskey` and the two PIN variants are only covered by the capability
  unit test, not by an exchange. Driving them needs a mock that picks an
  association model, which dbusmock's bluez5 template does not do.
- **`[f] Fix connection`** (§7). `tests/termdbus` asserts its presence in the
  footer on both transports, and that a BLE `[f]` against a disconnected host
  keeps the bond. What it *performs* needs a real link, so it belongs in
  `tests/btvirt`: the Classic unplug + unbond needs a bonded peer that can be
  re-paired afterwards (the shared-agent problem below), and the BLE repair needs
  a peer that observes the Service Changed indication and re-reads the Report Map
  — neither of which the `btvirt` peer does today.
- **`[ble] advertise`** (§4.1). Nothing asserts the adapter alias or Class of
  Device is actually set, or restored on exit. `tests/termdbus`' mock adapter
  could check `Alias`, but the class goes through the management socket, which
  only `tests/btvirt` has.
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

- **A pairing that lands while the Classic menu is scanning ends up one-sided,
  and bluez then refuses every retry.** Seen in `tests/twovm`'s D1 row
  (interactive, Classic), reproducibly: the host re-pairs while blooter's host
  picker is mid-inquiry, blooter's side completes SSP and stores a link key
  (`new_link_key_callback` on dev) while the host's controller reports
  `Simple Pairing Complete: Authentication Failure (0x05)`. From there it cannot
  recover on its own — dev now believes the device is paired, so bluez's
  `JustWorksRepairing = never` (the default) rejects the next Just Works
  confirmation without even asking the agent (`device_confirm_passkey`,
  `src/device.c`), and `bluetoothctl pair` reports neither success nor failure
  until it times out. Closing the menu with `[q]` first makes the same re-pair
  succeed every time. Two things to establish before fixing: whether the
  asymmetric SSP outcome is real or an artifact of btvirt's emulated controller
  (a BR/EDR inquiry does interfere with page scan, but a real radio may be more
  forgiving), and, if real, whether blooter should stop scanning while a pairing
  is in progress — its agent knows one is — or scan once rather than once per
  `wait_connected` cycle (CONNECTION.md §6.2). The D1 row documents the
  workaround it uses meanwhile.

- **`test_ble_unsubscribe_ends_session` is flaky, and fails outright in
  isolation.** Running `tests/btvirt/run.sh ble_unsubscribe` on its own fails
  every time — blooter never logs "host disconnected" within the 10 s window —
  while the full suite passes. Reproduced identically at `e0cc014`, so it
  predates the BLE peripheral rework and is a property of the test, not the
  transport: the suite pre-bonds the two controllers, and the earlier tests
  evidently leave state this one depends on. It has also failed once in a full
  run out of four. Worth pinning down before it masks a real teardown bug: the
  session ends off `notifier.stopped()` in `Shared::subscribe`
  (`transport/le.rs`), so the question is whether BlueZ always reports the CCCD
  session as stopped when an LE host vanishes, or only when it unsubscribes
  cleanly.
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
- **The BLE "fix connection" repair is unverified against real hosts.** The
  mechanism (CONNECTION.md §7.2b) is spec-correct, but which hosts act on it is
  not established: whether a BlueZ host's `input/hog-lib.c` re-reads the Report
  Map after a Database Hash mismatch or a Service Changed indication (as opposed
  to only after a fresh pairing), and the same question for Windows and Android.
  Worth a matrix of {BlueZ, Windows, Android} × {`slots` change, `axis_bits`
  change} × {automatic on reconnect, explicit `[f]`}. Where a host ignores it,
  the fallback is the manual re-pair blooter already prints — `[u]` to drop the
  bond here, then remove blooter from that host's Bluetooth settings. Note the
  matrix needs the host *connected* for `[f]` to do anything at all, since
  blooter cannot bring the link up itself (CONNECTION.md §4) — which is what
  `drop_connection` is for on BLE: it mutes the host rather than disconnecting
  it, so the menu opens over a link that is still up (§6.2). `tests/twovm`
  exercises exactly that path now; what is still unestablished is which *other*
  hosts act on the indication.

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
- **Outgoing HID (reconnect-initiate) connections** — implemented on Classic:
  when a reconnect target is set (the host menu, or `[connection] reconnect`),
  the Classic transport dials the host's HID L2CAP PSMs, raced against the
  inbound accept (design/CONNECTION.md §3.2, §6).
- **BLE outgoing connections** — implemented, then **removed**, and the reasoning
  is worth keeping: it never worked and could not have. HOGP puts the HID device
  in the GAP Peripheral role, so a host will not run its HOGP client over a link
  blooter opened; and `Device1.Connect()` takes no transport argument, so BlueZ
  picked the BR/EDR bearer for any dual-mode host and failed with
  `br-connection-unknown` before the role question even arose. The BLE menu is a
  bonded-host manager now rather than a host picker, and `[connection] reconnect`
  is Classic-only (design/CONNECTION.md §4, §6).
- **BLE pairing with no desktop agent** — fixed: the `accept` agent registered as
  `DisplayYesNo` rather than `NoInputNoOutput` (bluer derives the capability from
  which callbacks are set), so hosts chose Passkey Entry and blooter had no
  handler for it — a PIN on the host, nothing on blooter. BLE's `accept` agent is
  now callback-free, and `prompt` answers every association model on the TTY
  (§5).
- **The BLE device identity** — fixed: bluetoothd serves the GAP Device Name from
  the adapter alias and the Appearance from its Class of Device, so a host saw
  the machine's hostname and a computer icon whatever the advertisement said.
  `setup.rs` runs on BLE too now, under `[ble] advertise` (§4.1).
- **BLE as the default transport, with its own connection menu** — implemented:
  `[connection] protocol` defaults to `"ble"`. Both transports drive `menu.rs`,
  but for different jobs — Classic picks a host to pair and dial, BLE manages
  hosts already bonded (§6). Covered by `tests/termdbus` (the menu) and
  `tests/btvirt` (the link).
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
