# Architecture: blooter (Bluetooth HID device emulator)

This document describes the observable behaviour and internal architecture of
`blooter` — a Bluetooth Classic HID *device* emulator that makes a Linux box
appear to remote hosts as a Bluetooth keyboard + mouse (and, optionally, one or
more gamepads). It describes *what* the program does and how the pieces fit
together; it is a reference for maintenance, not a specification to implement
against.

blooter is built on Rust and [BlueR](https://crates.io/crates/bluer) (the
official Rust interface to BlueZ), and provides touchpad, gamepad,
configuration-file and adapter-setup support. The source is organized as:

| Module | Responsibility |
|---|---|
| `cli.rs` | Command-line parsing (§2). |
| `sdp.rs` | HID report descriptor and SDP service-record XML (§3). |
| `main.rs` | Runtime: config/session/input setup, transport selection, the shared accept/session loop, signals, shutdown (§4, §9). |
| `transport/` | The transport seam (§4): the `Transport` trait plus the classic (BR/EDR L2CAP) and LE (HID-over-GATT) implementations. |
| `input.rs` | Input sources — evdev scan, FIFO, gamepad slots, udev hotplug, `-l` listing (§6, §8). |
| `report.rs` | Session state and event → HID-report translation (§5, §7). |
| `keymap.rs` | Linux keycode ↔ HID usage tables and gamepad button/axis codes (§7.2, §7.4). |
| `config.rs` | TOML configuration: hotkey chords and gamepad options (§10). |
| `state.rs` | Per-host record of the descriptor each host bonded under (CONNECTION.md §7). |
| `setup.rs` | Adapter class/name/SSP setup and the interactive host menu (§10). |

The report *bytes* (`report.rs`) and the whole input pipeline are transport-agnostic; only delivery and discovery differ between Classic and LE. The transport is chosen by the `[connection] protocol` config key — `"classic"` (default) or `"ble"` (§4, §10).

## 1. Overview

The program:

1. Registers a Bluetooth HID (Human Interface Device Profile, UUID
   `00001124-0000-1000-8000-00805f9b34fb`) service record with `bluetoothd`
   over D-Bus, so remote hosts discover the machine as a keyboard/mouse (plus
   any advertised gamepad collections).
2. Optionally prepares the local adapter (Class of Device, name, Simple Secure
   Pairing), registers a pairing agent, and offers an interactive menu to pick a
   known host to (re)connect (§10; CONNECTION.md §5, §6).
3. Listens on the two standardized HID L2CAP PSMs — **0x11 (17, control)** and
   **0x13 (19, interrupt)** — and accepts one host connection at a time.
4. Reads Linux input events from `/dev/input/event*` devices (or from a FIFO),
   translates them into Boot-style HID input reports (keyboard, mouse, and one
   report per gamepad), and sends them to the connected host over the
   **interrupt** channel.
5. Provides configurable local hotkeys to drop the connection, terminate the
   program, and toggle input capture (§7.3).

It is primarily a **server/acceptor**. It can also **initiate** an outgoing HID
connection (reconnect-initiate) to a known host when a reconnect target is set —
via the interactive host menu (§10) or the `[connection] reconnect` config key —
racing the outbound dial against the inbound accept; see
[CONNECTION.md](CONNECTION.md) §3.2, §6. Absent a target it only accepts.

External preconditions:

- (Classic transport only) `bluetoothd` must run with its own `input` plugin
  disabled (`bluetoothd -P input`), otherwise the HID UUID is already claimed and
  profile registration fails with `org.bluez.Error.NotPermitted`. The LE
  transport (`protocol = "ble"`) registers a GATT server instead and does not
  need this.
- (Classic transport only) Binding the HID L2CAP PSMs (0x11 / 0x13) requires
  privilege (they are below 0x1001): run as root or grant `CAP_NET_BIND_SERVICE`.
- Adapter setup (§10) additionally needs `CAP_NET_ADMIN`; it is skipped with a
  warning if unavailable, and can be turned off entirely with `-n`.
- The user needs read access to the `/dev/input/event*` devices used.

## 2. Command-line interface

```
blooter [-h|-?|--help] [-s|--skipsdp] [-n|--nosetup] [-e<NUM>]... [-f<NAME>] [-l] [-x] [-c<FILE>] [-d]
```

| Option | Behavior |
|---|---|
| `-h`, `-?`, `--help` | Print usage help, exit 0. |
| `-e<NUM>` | Restrict input to event device number `<NUM>` (`/dev/input/event<NUM>`). May be given multiple times; the selections accumulate. Default: open every readable keyboard, pointer (mouse/trackpoint), touchpad and gamepad device — other devices (power buttons, lid switches, jack-detect, …) are skipped. |
| `-f<NAME>` | FIFO mode: instead of event devices, read raw `struct input_event` records from a FIFO at path `<NAME>` (created with mode `0600` if absent). Mutually exclusive in effect with `-e`/`-x`; disables gamepad forwarding. |
| `-l` | List available input devices (§8) and exit. |
| `-x` | Grab opened event devices exclusively (`EVIOCGRAB`) so events do not reach the local session. Grabs are released while input capture is toggled off (§7.3) and on exit. |
| `-c<FILE>` | Read configuration from `<FILE>` instead of the default search path (§10). |
| `-s`, `--skipsdp` | Skip the D-Bus profile/SDP registration (debugging only). Classic only. |
| `-n`, `--nosetup` | Skip adapter setup (class `0x0540`, name `blooter`, SSP) and the interactive host menu (§10). |
| `-d` | Enable debug logging of raw input events and socket traffic. |
| anything else | Print `Invalid argument: '<arg>'`, exit 1. |

Both the attached forms (`-e3`, `-fmyfifo`, `-cpath`) and the separated forms
(`-e 3`, `-f myfifo`, `-c path`) are accepted. Logging verbosity can also be
tuned with `RUST_LOG` (e.g. `RUST_LOG=debug`); `-d` is a shortcut for the debug
level.

Exit codes: `0` success; `1` argument/registration/config error; `2` input-open
or async-runtime failure; `3` L2CAP bind failure. Any startup failure prints a
clear message.

## 3. Service registration

The HID profile is registered with `bluetoothd` via
`org.bluez.ProfileManager1.RegisterProfile` (BlueR `Session::register_profile`
with a `Profile`):

- **UUID:** `00001124-0000-1000-8000-00805f9b34fb` (HID).
- **Name:** `blooter HID`.
- **Role:** `server`.
- **RequireAuthentication / RequireAuthorization:** `false`.
- **ServiceRecord:** the XML SDP record in §3.1.

The returned profile handle is owned by a background task for the program's
lifetime; dropping it (on shutdown) unregisters the profile. `bluetoothd` calls
`NewConnection` on the profile object when a host connects to the UUID — blooter
**rejects** every such request (`ReqError::Rejected`). The profile registration
exists only to publish the SDP record; the real traffic is handled on our own
L2CAP listeners (§4). With `-s`/`--skipsdp` the registration is skipped
entirely.

### 3.1 SDP record

The `ServiceRecord` XML (built by `sdp::service_record_xml`) contains these
attributes (BlueZ record-XML syntax):

| Attr | Name | Value |
|---|---|---|
| `0x0001` | ServiceClassIDList | sequence: uuid `0x1124` (HID) |
| `0x0004` | ProtocolDescriptorList | sequence: ( uuid `0x0100` L2CAP, uint16 `0x0011` PSM ), ( uuid `0x0011` HIDP ) |
| `0x0005` | BrowseGroupList | sequence: uuid `0x1002` (public browse group) |
| `0x0006` | LanguageBaseAttributeIDList | uint16 `0x656e` ("en"), uint16 `0x006a` (UTF-8), uint16 `0x0100` |
| `0x0009` | ProfileDescriptorList | sequence: ( uuid `0x1124`, uint16 `0x0100` = HID v1.00 ) |
| `0x000d` | AdditionalProtocolDescriptorLists | sequence: ( ( uuid `0x0100`, uint16 `0x0013` PSM ), ( uuid `0x0011` HIDP ) ) |
| `0x0100` | ServiceName | `Bluez virtual Mouse and Keyboard` |
| `0x0101` | ServiceDescription | `Keyboard` |
| `0x0102` | ProviderName | `blooter` |
| `0x0200` | HIDDeviceReleaseNumber | uint16 `0x0100` |
| `0x0201` | HIDParserVersion | uint16 `0x0111` |
| `0x0202` | HIDDeviceSubclass | uint8 `0x40` (keyboard) |
| `0x0203` | HIDCountryCode | uint8 `0x00` |
| `0x0204` | HIDVirtualCable | `true` |
| `0x0205` | HIDReconnectInitiate | `true` |
| `0x0206` | HIDDescriptorList | sequence: ( uint8 `0x22` (report descriptor), text encoding="hex" = hex dump of the descriptor in §3.2 ) |
| `0x0207` | HIDLANGIDBaseList | sequence: ( uint16 `0x0409` en-US, uint16 `0x0100` ) |
| `0x020b` | HIDProfileVersion | uint16 `0x0100` |
| `0x020e` | HIDBootDevice | `false` |

### 3.2 HID report descriptor

The descriptor is assembled dynamically by `sdp::report_descriptor(n_gamepads)`:
a fixed **98-byte base** (mouse + keyboard) followed by one **85-byte gamepad
collection** per advertised controller.

**Base (98 bytes):**

```
05 01 09 02 A1 01 85 01 09 01 A1 00
05 09 19 01 29 03 15 00 25 01 75 01
95 03 81 02 75 05 95 01 81 01 05 01
09 30 09 31 09 38 15 81 25 7F 75 08
95 03 81 06 C0 C0 05 01 09 06 A1 01
85 02 A1 00 05 07 19 E0 29 E7 15 00
25 01 75 01 95 08 81 02 95 08 75 08
15 00 25 65 05 07 19 00 29 65 81 00
C0 C0
```

- **Usage Page Generic Desktop, Usage Mouse, Collection Application**
  - Report ID **1**; Usage Pointer, Collection Physical
    - Buttons 1–3: 3 × 1-bit input (Data,Var,Abs), logical 0–1, then 5 bits
      constant padding
    - Usage X, Y, Wheel: 3 × 8-bit input (Data,Var,**Rel**), logical −127…127.
      (Report Count is **3** here, matching the three wire axis bytes; the C
      original declared 2. This does not change the wire format in §5.)
- **Usage Page Generic Desktop, Usage Keyboard, Collection Application**
  - Report ID **2**; Collection Physical
    - Modifier byte: usages Keyboard `0xE0`–`0xE7`, 8 × 1-bit (Data,Var,Abs)
    - Key array: 8 × 8-bit array items (Data,Array), usages `0x00`–`0x65`
    - No LED output report — the device never receives LED state.

**Gamepad collection (85 bytes each, `sdp::gamepad_block`):** a standard
Generic-Desktop Gamepad application collection with Report ID **3, 4, …** (base
`GAMEPAD_REPORT_ID_BASE = 3`):

- 16 buttons: 16 × 1-bit input (Data,Var,Abs)
- Hat switch: 4-bit null-capable input (logical 0–7, physical 0–315°) + 4-bit
  constant padding
- Sticks X, Y, Rx, Ry: 4 × 8-bit input (Data,Var,Abs), logical 0–255
- Triggers Z, Rz: 2 × 8-bit input (Data,Var,Abs), logical 0–255

Report IDs 1 (mouse), 2 (keyboard) and 3+ (gamepads) and the wire formats in §5
are kept in sync with this descriptor.

**Hosts cache this descriptor.** A remote host reads the SDP record once, when it
bonds, and keeps it for the lifetime of the bond — so changing the descriptor
(i.e. changing the advertised gamepad slot count) is invisible to hosts that
already paired, and the new layout silently never appears on them. blooter
fingerprints the descriptor and offers a "fix connection" action to clear a host's
cached copy; see CONNECTION.md §7.

## 4. Transports

blooter drives one `Transport` (`transport/` module) chosen at startup by the
`[connection] protocol` config key (§10): the default **Classic** L2CAP
transport (§4.1, `"classic"`) or the **LE** HID-over-GATT transport (§4.2,
`"ble"`). Both share the same accept → session loop in `main.rs`: wait for a host,
reset per-session state, forward translated reports (§5, §7) until the host
disconnects or a hotkey/signal fires, then return to accepting. The report bytes
are identical; only how they are delivered and how the device is discovered
differ.

### 4.1 Classic (BR/EDR) L2CAP

- Two listening sockets, both `SOCK_SEQPACKET`, `BTPROTO_L2CAP`, bound to
  `BDADDR_ANY` (BR/EDR), PSM **0x11** (control) and PSM **0x13** (interrupt),
  via `bluer::l2cap::SeqPacketListener::bind`. Binding to these PSMs (< 0x1001)
  requires `CAP_NET_BIND_SERVICE` or root.
- Only one host session at a time.
- **Accept order:** wait for a connection on the *control* PSM first. Once
  accepted, wait up to **3 seconds** for the same host to connect on the
  *interrupt* PSM. If it doesn't arrive in time, log an error and go back to
  waiting on control.
- **Reconnect-initiate:** if a reconnect target is set (§10, CONNECTION.md §3.2)
  — always an *already-bonded* host — an outbound dial of the host's control +
  interrupt PSMs is raced against the inbound accept (with exponential backoff on
  failure). blooter never initiates pairing in the *background* — a new host is
  bonded via its incoming connection, or deliberately from the interactive menu
  (§10.2) — so the background dial only ever re-links an already-bonded host.
  Whichever completes first wins; the target is cleared once any link is up, so an
  intentional drop or link loss does not immediately redial.
- While waiting to accept, input events are still consumed to keep modifier
  state current and to honour the exit hotkey; signals are honoured too.
- On success, log the peer's Bluetooth address and enter the connected state.
- The control channel is read during a session and answered minimally:
  `VIRTUAL_CABLE_UNPLUG` (0x15) or EOF on either channel is treated as a
  disconnect; other HIDP transfer requests (GET/SET_REPORT, GET/SET_PROTOCOL,
  …) get a `HANDSHAKE ERR_UNSUPPORTED_REQUEST` (0x03) reply so the socket
  buffer cannot fill.
- All input reports are sent on the **interrupt** channel. A failed send (peer
  gone) ends the session: both channels close and blooter returns to waiting.
  Socket writes return `Err` rather than raising `SIGPIPE`.
- After a session ends, sleep **0.5 s** before accepting again (avoids
  reconnect flooding), then loop.

### 4.2 LE (HID-over-GATT / HOGP)

With `[connection] protocol = "ble"`, blooter presents itself through the
**HID-over-GATT Profile (HOGP)** instead of Classic HID. It registers a GATT
server (`org.bluez.GattManager1`) and an LE advertisement
(`org.bluez.LEAdvertisingManager1`) via BlueR's `gatt` and `adv` modules, and
never touches the classic HID profile, the L2CAP PSMs or the Class of Device.
The HID report *bytes* (§5) are unchanged: only delivery (GATT **Notify** rather
than the interrupt channel) and discovery (the GATT tree + LE advertising rather
than SDP) differ.

Because the GATT path does not claim the classic HID UUID or the HID L2CAP PSMs,
LE mode does **not** require `bluetoothd -P input` and does **not** need
`CAP_NET_BIND_SERVICE`. (bluetoothd's own `input` plugin acts as a GATT *client*
to remote HID devices and does not contend for our server registration.) The
adapter must be powered — blooter powers it — and, for a simple single-host
device, one advertisement set per adapter suffices.

#### GATT services

- **HID service (`0x1812`)**:
  - **HID Information (`0x2A4A`)** — read: bcdHID `0x0111`, country code `0`,
    flags = NormallyConnectable.
  - **Report Map (`0x2A4B`)** — read: the HID report descriptor bytes, exactly
    the output of `sdp::report_descriptor(n_gamepads)` (the same descriptor the
    classic path embeds in its SDP record, §3.2; the SDP *XML* is classic-only).
  - **Report (`0x2A4D`)** — one instance per report blooter sends: mouse (id 1),
    keyboard (id 2), and each gamepad (id 3+). Each is Read + **Notify**, has a
    **Report Reference** descriptor (`0x2908` = `[report_id, type=Input(1)]`) and
    a **CCCD** (`0x2902`, added automatically by BlueZ for a notify
    characteristic). Reads require an encrypted link.
  - **Protocol Mode (`0x2A4E`)** — Read + WriteWithoutResponse: reports Report
    Protocol (`0x01`), the only supported mode; writes (boot-mode switching) are
    accepted and ignored.
  - **HID Control Point (`0x2A4C`)** — WriteWithoutResponse: suspend/exit-suspend
    are accepted and ignored, mirroring the classic "ignore control features"
    stance.
- **Device Information service (`0x180A`)** — read-only: PnP ID (`0x2A50`, USB
  vendor source, VID `0x1D6B`, PID `0x0001`, version 1.0.0), Manufacturer Name
  (`0x2A29`, `blooter`) and Model Number (`0x2A24`, `blooter HID`).
- **Battery service (`0x180F`)** — a constant 100% Battery Level (`0x2A19`),
  Read + Notify (mandatory HOGP companion; blooter never pushes a notification).

#### Advertising, security and sessions

- **Advertising:** advertisement type Peripheral, the HID service UUID
  (`0x1812`), local name `blooter`, Keyboard **Appearance** (`0x03C1`) — a combo
  keyboard/mouse device advertises the keyboard icon, matching the identity the
  classic transport sets as its Class of Device — and discoverable/connectable.
  This replaces the Class-of-Device and adapter-name logic of `setup.rs`, which
  is not run in LE mode.
- **Pairing / bonding:** HOGP requires an encrypted, bonded link before reports
  flow. The shared BlueZ pairing agent (§10.2, CONNECTION.md §5) — registered as
  the default agent, with the adapter set pairable — lets a first-time host bond
  (silently, or with a TTY confirmation per `[connection] pairing`). BlueZ
  persists the bond, so a bonded host reconnects without re-pairing.
- **Sending a report** is a GATT **Notify** on the Report characteristic for that
  report id (the `0xA1` HIDP header and report-id byte are stripped from the
  wire bytes of §5; the id selects the characteristic, the remainder is the
  notification value). Notifications are best-effort/unacknowledged, matching the
  classic interrupt-channel semantics.
- **Connection tracking:** a host is "connected" once it subscribes to any Report
  characteristic's CCCD. On the first connect blooter pushes initial zeroed
  reports so the host has state; `send_report` no-ops for any report the host has
  not subscribed to. The session ends when the last subscription is dropped
  (unsubscribe or link loss).
- **Out of scope (as for classic, TODO.md):** output reports (keyboard LEDs) and
  boot-protocol mode; more than one bonded host at a time; and advertising
  Classic and LE simultaneously as one logical device (the transport is chosen
  at launch).

## 5. Wire format of input reports

Every report is prefixed with the HIDP header byte `0xA1` (HIDP `DATA`
transaction, report type Input).

**Mouse report — 6 bytes:**

| Byte | Meaning |
|---|---|
| 0 | `0xA1` |
| 1 | Report ID = `1` |
| 2 | Buttons: bit 0 left, bit 1 right, bit 2 middle; bits 3–7 zero |
| 3 | X: relative motion, signed 8-bit (clamped to −127…127) |
| 4 | Y: relative motion, signed 8-bit |
| 5 | Wheel: relative scroll, signed 8-bit |

**Keyboard report — 11 bytes:**

| Byte | Meaning |
|---|---|
| 0 | `0xA1` |
| 1 | Report ID = `2` |
| 2 | Modifier bitmap (bit 0 LCtrl, 1 LShift, 2 LAlt, 3 LMeta/GUI, 4 RCtrl, 5 RShift, 6 RAlt, 7 RMeta) |
| 3–10 | Up to 8 currently-pressed HID usage codes (Keyboard/Keypad page), zero-filled; earliest-pressed first |

**Gamepad report — 11 bytes** (one per advertised controller):

| Byte | Meaning |
|---|---|
| 0 | `0xA1` |
| 1 | Report ID = `3 + slot` (slot 0 → 3, slot 1 → 4, …) |
| 2 | Buttons, low byte (bits 0–7) |
| 3 | Buttons, high byte (bits 8–15) |
| 4 | Hat switch: `0`=N, `1`=NE, `2`=E, `3`=SE, `4`=S, `5`=SW, `6`=W, `7`=NW, `8`=centred |
| 5 | Left stick X, 0–255 (centre 128) |
| 6 | Left stick Y, 0–255 (centre 128) |
| 7 | Right stick X, 0–255 (centre 128) |
| 8 | Right stick Y, 0–255 (centre 128) |
| 9 | Left trigger, 0–255 |
| 10 | Right trigger, 0–255 |

Each gamepad report is a full snapshot, re-sent whenever any control on that
controller changes.

## 6. Input sources

### 6.1 Event-device mode (default)

- Scan `/dev/input/event0` … `/dev/input/event63` (`MAX_EVDEVS = 64`).
- If any `-e<NUM>` was given, open exactly the selected numbers (warning on any
  that fail to open). Otherwise open every device that both opens read-only and
  passes the **relevance filter**: a device is relevant if it is a keyboard
  (has `KEY_A`), a relative pointer (`EV_REL` + `BTN_LEFT`), a touchpad
  (`EV_ABS` + `BTN_TOUCH`), or a gamepad. Irrelevant nodes (power buttons, lid
  switches, …) are silently skipped. Each opened device is logged.
- A device is a **gamepad** if it has absolute axes and `BTN_SOUTH`. Each
  gamepad claims a free advertised slot (§6.4) and is forwarded as its own HID
  controller; gamepads with no free slot are left alone.
- Each device runs its own async reader task (`evdev` event streams) feeding a
  shared mpsc channel of `RawEvent`s.
- With `-x`, each opened device is grabbed exclusively (`EVIOCGRAB`); a failed
  grab is logged and the device is still used. Grabs are released (and, for
  touchpads, a neutral "all fingers up" sequence is injected so libinput
  resumes cleanly) when capture is toggled off (§7.3) and on exit.
- At least one device must open successfully, or blooter exits with an error.

### 6.2 FIFO mode (`-f`)

- If the path exists it must already be a FIFO, else error out. If absent, it is
  created with `mkfifo`, mode `0600`.
- Read on a blocking thread as raw native-endian `struct input_event` records
  (24 bytes on 64-bit: `timeval` (16) + `u16 type` + `u16 code` + `s32 value`),
  bridged onto the async channel. Treated as event device #0. On EOF (no
  writer) the FIFO is reopened and polling continues. Gamepad forwarding is
  disabled in FIFO mode.

### 6.3 Event draining

Events that arrive while **no host is connected** are still consumed (to keep
modifier/button state current for the hotkeys in §7.3) but not transmitted, as
transmission is gated on the connected state. Immediately before entering the
connected state, the pending-event channel is drained so stale keystrokes are
not delivered to the new host.

### 6.4 Gamepad slots and hotplug

The number of advertised gamepad controllers is fixed when blooter registers
its profile (the descriptor cannot change while running) and is decided by
`[gamepad] slots` (§10):

- `"initial"` (default) — one controller per gamepad present at startup;
- `0` — gamepad forwarding disabled;
- `N` — exactly `N` controllers; startup gamepads fill slots in ascending
  device order, extra slots stay idle.

A lock-free `SlotPool` (an atomic bitmask) tracks which slots are occupied.
Each gamepad reader claims the lowest free slot on open and frees it when the
task ends (read error, unplug/EOF, or shutdown). When `[gamepad] hotplug` is
active (§10), a **udev monitor** on the `input` subsystem opens gamepads plugged
in after startup into any free slot; with an explicit `-e` selection it only
opens listed event numbers. Requires system **libudev**.

## 7. Event translation

Per-session state lives in `report::InputState` and is reset on every new
connection: `mouse_buttons: u8`, `modifiers: u8`, `pressed_keys: [u8; 8]`, a
`capture` flag, touchpad tracking state, and one `GamepadState` per advertised
slot. Reports are only emitted while `capture` is true; state is tracked
regardless, so hotkey chords keep working with capture off.

### 7.1 `EV_KEY`

- **Mouse buttons** `BTN_LEFT`/`BTN_RIGHT`/`BTN_MIDDLE`: set/clear bit 0/1/2 of
  `mouse_buttons`; emit a mouse report with the new buttons and zero axes.
- **`BTN_TOUCH`**: update touchpad finger-down state and reset the motion
  reference (§7.2b); no report.
- **Modifier keys** `KEY_LEFTCTRL`, `KEY_LEFTSHIFT`, `KEY_LEFTALT`,
  `KEY_LEFTMETA`, `KEY_RIGHTCTRL`, `KEY_RIGHTSHIFT`, `KEY_RIGHTALT`,
  `KEY_RIGHTMETA` → bits 0–7 of `modifiers`. Set on value ≥ 1, clear on 0; emit
  a keyboard report.
- **Regular keys** (§7.4):
  - value 1 (press): append the HID usage to the first free `pressed_keys` slot
    (no-op if already present or all 8 full — no rollover-error reporting).
  - value 0 (release): remove the usage, shifting later entries left.
  - value 2 (autorepeat): no list change, but a report is still emitted.
  - Emit a keyboard report with current `modifiers` + `pressed_keys`.
- Hotkey trigger keys (§7.3) are handled first and never forwarded.
- Keys with no mapping are ignored.

### 7.2 `EV_REL`

Relative motion, coalesced per event (no accumulation):

- `REL_X` (0) → mouse report with X = value, Y = 0, wheel = 0.
- `REL_Y` (1) → Y = value.
- `REL_WHEEL` (8) → wheel = value.
- Values are clamped to −127…127. The button byte always reflects current
  `mouse_buttons`.

### 7.2b `EV_ABS` (touchpad)

Touchpads report absolute finger positions; blooter derives relative motion:

- Motion is ignored unless a finger is down (`BTN_TOUCH`).
- The first `ABS_X`/`ABS_Y` position after touch-down only seeds the reference
  (no jump on finger landing); subsequent positions emit a mouse report with
  the per-axis delta (clamped to −127…127). Lifting the finger resets the
  reference.

Gamepad `EV_ABS` events (sticks, triggers, D-pad) are routed by slot to §7.5.
All other event types (`EV_SYN`, `EV_MSC`, `EV_LED`, …) are ignored.

### 7.3 Local hotkeys and input capture

Hotkeys are configurable chords (§10); the built-in defaults are:

- **Scroll Lock** — drop the current host connection (return to accepting).
- **Ctrl + Alt + Scroll Lock** — terminate blooter cleanly (§9).
- **Shift + Scroll Lock** — toggle input capture. While capture is off, nothing
  is forwarded (an all-keys-up report and neutral gamepad reports are sent
  first so nothing stays latched host-side), and `-x` exclusive grabs are
  released so input reaches the local session again. A new host connection
  re-enables capture. Separate `capture_on`/`capture_off` chords exist but are
  disabled by default.

A chord fires when its trigger key is **released** while the required modifiers
are held; trigger keys are consumed locally and never forwarded. When several
chords share a trigger, the most specific (most modifiers) wins. Dropping the
connection first sends an all-keys-up keyboard report and neutral gamepad
reports.

### 7.4 Key mapping (Linux keycode → HID usage)

The standard Linux-input-to-USB-HID correspondence for the Boot keyboard usage
range (Keyboard/Keypad page, usages 4–99):

| Linux key(s) | HID usage |
|---|---|
| `KEY_A` … `KEY_Z` | 4 … 29 (alphabetical) |
| `KEY_1` … `KEY_9`, `KEY_0` | 30 … 39 |
| `KEY_ENTER`, `KEY_ESC`, `KEY_BACKSPACE`, `KEY_TAB`, `KEY_SPACE` | 40, 41, 42, 43, 44 |
| `KEY_MINUS`, `KEY_EQUAL`, `KEY_LEFTBRACE`, `KEY_RIGHTBRACE`, `KEY_BACKSLASH`, `KEY_102ND` | 45, 46, 47, 48, 49, 50 |
| `KEY_SEMICOLON`, `KEY_APOSTROPHE`, `KEY_GRAVE`, `KEY_COMMA`, `KEY_DOT`, `KEY_SLASH`, `KEY_CAPSLOCK` | 51, 52, 53, 54, 55, 56, 57 |
| `KEY_F1` … `KEY_F12` | 58 … 69 |
| `KEY_SYSRQ` (PrintScreen), `KEY_PAUSE` | 70, 72 *(71 = Scroll Lock, the default hotkey trigger)* |
| `KEY_INSERT`, `KEY_HOME`, `KEY_PAGEUP`, `KEY_DELETE`, `KEY_END`, `KEY_PAGEDOWN` | 73, 74, 75, 76, 77, 78 |
| `KEY_RIGHT`, `KEY_LEFT`, `KEY_DOWN`, `KEY_UP` | 79, 80, 81, 82 |
| `KEY_NUMLOCK`, `KEY_KPSLASH`, `KEY_KPASTERISK`, `KEY_KPMINUS`, `KEY_KPPLUS`, `KEY_KPENTER` | 83, 84, 85, 86, 87, 88 |
| `KEY_KP1` … `KEY_KP9`, `KEY_KP0`, `KEY_KPDOT` | 89 … 97, 98, 99 |

Everything else (media keys, `KEY_MENU`, …) is unmapped/ignored, matching the
usage range 0–0x65 declared in the report descriptor. A hotkey trigger is never
forwarded regardless of its mapping.

### 7.5 Gamepad events

Events carrying a gamepad slot update that slot's `GamepadState` and (while
capturing) emit its full report (§5):

- **Buttons** (`BTN_SOUTH`, `BTN_EAST`, … the `BTN_GAMEPAD` range) map to bits
  0–15 of the 16-bit button field.
- **Sticks / triggers** (`ABS_X`/`ABS_Y`/`ABS_RX`/`ABS_RY`/`ABS_Z`/`ABS_RZ`)
  are normalized by the reader from the device's reported axis range to 0–255
  before translation; sticks centre at 128.
- **D-pad** (`ABS_HAT0X`/`ABS_HAT0Y`, each −1/0/1) is combined into the 8-way
  HID hat value (8 = centred).

Events for an out-of-range slot are ignored.

## 8. Device listing (`-l`)

Print a table of `/dev/input/event0..63`:

```
List of available input devices:
num	Vendor/Product, Name, -x compatible (+/-), * = default scan
 3	[046d:c31c.0111] 'Logitech Keyboard' (+)*
```

Per device: index, `[vendor:product.version]` from `EVIOCGID`, the device name
from `EVIOCGNAME`, `+`/`-` for whether a test `EVIOCGRAB` succeeds (grab then
immediately ungrab), and a trailing `*` if the default scan (no `-e`) would pick
the device up. Devices failing with `EACCES` are listed as `[permission
denied]`; unopenable numbers are skipped (numbering can have gaps). Exit 0. This
needs no async runtime or Bluetooth.

## 9. Lifecycle, signals, shutdown

- **Startup order:** parse args → load config → decide gamepad slot count →
  connect to `bluetoothd` and register the profile (unless `-s`) → adapter
  setup (unless `-n`) → open input sources → bind + listen both PSMs → install
  signal handlers → optional interactive host menu (if stdin is a TTY, §10) →
  print `The HID-Client is now ready to accept connections from another
  machine` → main accept/session loop (§4).
- **SIGTERM / SIGHUP:** always request shutdown.
- **SIGINT (Ctrl+C):** requests shutdown only when *no* host is connected;
  ignored during a session (the keystroke may be meant for the remote side).
- **Clean shutdown:** abort the profile task (dropping the handle unregisters
  the profile); close the capture watch channel so each reader releases its
  grab (injecting the touchpad reset) and closes its fd; wait briefly for
  readers to finish; restore adapter class/name/SSP (`BtSetup::drop`); flush
  pending stdin if it is a TTY (so forwarded keystrokes do not spill into the
  terminal); print `blooter stopped.`.

## 10. Configuration and adapter setup

### 10.1 Configuration file

A TOML file, looked up in order: the `-c<FILE>` path;
`$XDG_CONFIG_HOME/blooter/config.toml` (falling back to
`~/.config/blooter/config.toml`); `/etc/blooter/config.toml`. If none exists the
built-in defaults apply. `config.example.toml` documents every key with its
default value commented out.

- **`[hotkeys]`** — `drop_connection`, `exit`, `capture_toggle`, `capture_on`,
  `capture_off`. Each value is a chord: zero or more modifiers plus a final
  trigger key, joined with `+` (e.g. `"leftcontrol+leftalt+scrolllock"`), fired
  when the trigger is released while the modifiers are held. Key names follow
  [keyd](https://github.com/rvaiya/keyd) (`scrolllock`, `pause`, `f12`,
  `kpenter`, `leftmeta`, …); the side-agnostic aliases `control`/`ctrl`,
  `shift`, `alt`, `meta`/`super` match either side. `""` disables a hotkey.
- **`[gamepad] slots`** — `"initial"` (default), `0`, or a fixed count `N`
  (§6.4).
- **`[gamepad] hotplug`** — `"auto"` (default: on iff `slots` is a fixed count
  > 0), `"on"` (always monitor), or `"off"` (§6.4).
- **`[connection] protocol`** — `"classic"` (default, BR/EDR HID) or `"ble"`
  (Bluetooth Low Energy / HOGP). Selects the transport (§4).
- **`[connection] pairing`** — `"auto"` (silent "Just Works") or `"confirm"`
  (prompt on the TTY). Absent → inferred: `confirm` interactively, else `auto`
  (CONNECTION.md §5).
- **`[connection] reconnect`** — a host address `"AA:BB:CC:DD:EE:FF"` to initiate
  an outgoing HID connection to (Classic only; CONNECTION.md §3.2, §6). Absent →
  accept-only unless the host menu supplies a target.

Parse errors report the offending line and abort startup (exit 1).

### 10.2 Adapter setup and host menu (`setup.rs`)

Unless `-n` is given and if a default adapter is present, blooter talks to the
BlueZ **management socket** (`AF_BLUETOOTH`/`HCI_CHANNEL_CONTROL`) to:

- save the adapter's current Class of Device, local name and SSP mode;
- set Class of Device `0x05/0x40` (peripheral major / keyboard minor), local
  name `blooter`, and enable Simple Secure Pairing — so hosts recognise and
  pair with the machine easily.

The saved settings are restored when the `BtSetup` guard drops (shutdown).
Management commands need `CAP_NET_ADMIN`; if unavailable, setup is skipped with
a warning.

A shared BlueZ **pairing agent** is registered as the default agent for both
transports (previously LE-only; Classic had none, so an incoming pair could
stall) and the adapter is set pairable. Its behaviour follows `[connection]
pairing`: auto-accept or TTY confirm (CONNECTION.md §5). In Classic mode the
adapter is also made **discoverable** (its prior state restored on exit), and
blooter prints that it is now visible, so a host can find and connect to it.

When stdin is a TTY, an interactive **host menu** runs **concurrently** with the
accept loop (CONNECTION.md §6): a short (~4 s) discovery pass, then a list of
known devices (connected first, then paired, then unpaired; each strongest-signal
first). Selecting one pairs it from here if it is new, then the Classic transport
initiates the outgoing HID connection to it (CONNECTION.md §3.2). Because the menu
is concurrent, an **incoming** connection that arrives while it is open is taken
as the user's choice: blooter uses it and closes the menu with a note. Pressing
Enter skips, leaving blooter accepting (plus dialing any bonded `[connection]
reconnect` target).
