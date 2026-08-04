# Architecture: blooter (Bluetooth HID device emulator)

This document describes the observable behaviour and internal architecture of
`blooter` — a Bluetooth HID *device* emulator that makes a Linux box
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
| `transport/` | The transport seam (§4): the `Transport` trait, the outgoing report buffer (§7.2c), plus the classic (BR/EDR L2CAP) and LE (HID-over-GATT) implementations. |
| `input.rs` | Input sources — evdev scan, FIFO, gamepad slots, udev hotplug, `-l` listing (§6, §8). |
| `report.rs` | Session state and event → HID-report translation, including pointer accumulation (§5, §7). |
| `keymap.rs` | Linux keycode ↔ HID usage tables and gamepad button/axis codes (§7.2, §7.4). |
| `config.rs` | TOML configuration: hotkey chords, gamepad and pointer options (§10). |
| `state.rs` | Per-host record of the descriptor each host bonded under (CONNECTION.md §7). |
| `setup.rs` | Adapter class/name/SSP setup and the interactive host menu (§10). |

The report *bytes* (`report.rs`) and the whole input pipeline are transport-agnostic; only delivery and discovery differ between Classic and LE. The transport is chosen by the `[connection] protocol` config key — `"ble"` (default) or `"classic"` (§4, §10).

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

The descriptor is assembled dynamically by `sdp::report_descriptor(layout)`,
where `sdp::Layout` is the gamepad slot count, the axis width and whether the
TV-remote collection is on: a **mouse collection** in one of two axis widths, the
fixed **44-byte keyboard collection**, one **85-byte gamepad collection** per
advertised controller, and finally — with `[remote] enabled` — the **25-byte
Consumer Control collection** (REMOTE.md §3). With 8-bit axes the mouse
collection is 54 bytes, so the base is the original **98 bytes**; with 16-bit
axes (§7.2c) it is 66 bytes, for a **110-byte** base.

**Base (98 bytes, `axis_bits = 8`):**

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

**Mouse collection with `axis_bits = 16` (66 bytes):** identical up to the
5-bit button padding, then X and Y become one 16-bit Input item and the Wheel a
separate 8-bit one:

```
09 30 09 31 16 01 80 26 FF 7F 75 10
95 02 81 06
09 38 15 81 25 7F 75 08 95 01 81 06
```

- Usage X, Y: 2 × 16-bit input (Data,Var,**Rel**), logical −32767…32767.
- Usage Wheel: 1 × 8-bit input (Data,Var,**Rel**), logical −127…127.

Two Input items are required because the fields differ in Report Size and
logical range; Usage items are consumed by the Input item that follows them, so
Usage Wheel is declared *after* the X/Y item (the same shape the gamepad
collection uses for its sticks and triggers). Total input size is
3 + 5 + 16 + 16 + 8 = 48 bits, so the 5-bit padding is what keeps the 16-bit
fields byte-aligned. The Report Count quirk above does not arise here: each
item's count equals its own usage count by construction.

The axis width is part of the descriptor, so it feeds `descriptor_fingerprint`
and a bonded host holding the other width is flagged for `[f] Fix connection`
on either transport (CONNECTION.md §7). It is the case the BLE repair is built
around: it changes only the *value* of the Report Map, leaving the GATT database
and its handles identical, which is why the fingerprint is also carried in a
vendor characteristic's UUID (§4.2, CONNECTION.md §7.2b).
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

**Consumer Control collection (25 bytes, `sdp::consumer_block`):** present only
with `[remote] enabled`, and emitted *after* every gamepad so that enabling it
shifts no other report ID and leaving it off reproduces the descriptor
byte-for-byte. Its report ID is therefore `3 + n_gamepads`. One 16-bit array
item, logical and usage range `0x0000`–`0x02A2`, Report Count 1: the report
carries the usage code of the single remote button held, `0x0000` for none. See
REMOTE.md §3 for the byte listing and §2 for which usages a host acts on.

Report IDs 1 (mouse), 2 (keyboard), 3+ (gamepads) and `3 + n_gamepads`
(consumer) and the wire formats in §5 are kept in sync with this descriptor.

**Hosts cache this descriptor.** A remote host reads it once, when it bonds — the
SDP record on Classic, the Report Map characteristic on BLE — and keeps it for
the lifetime of the bond, so changing the descriptor (i.e. changing the
advertised gamepad slot count, the axis width or the remote) is invisible to hosts that
already paired, and the new layout silently never appears on them — the same
applies to turning `[remote]` on (REMOTE.md §3.2). blooter
fingerprints the descriptor and offers a "fix connection" action to clear a host's
cached copy; see CONNECTION.md §7.

## 4. Transports

blooter drives one `Transport` (`transport/` module) chosen at startup by the
`[connection] protocol` config key (§10): the default **LE** HID-over-GATT
transport (§4.2, `"ble"`) or the **Classic** L2CAP transport (§4.1,
`"classic"`). Both share the same accept → session loop in `main.rs`: wait for a host,
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
    the output of `sdp::report_descriptor(layout)` (the same descriptor the
    classic path embeds in its SDP record, §3.2; the SDP *XML* is classic-only).
  - **Report (`0x2A4D`)** — one instance per report blooter sends: mouse (id 1),
    keyboard (id 2), each gamepad (id 3+) and, with `[remote] enabled`, the
    consumer collection (id `3 + n_gamepads`). Each is Read + **Notify**, has a
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
- **Layout service (`626c6f74-6572-4c41-594f-5554…`)** — one read-only vendor
  characteristic whose UUID carries the descriptor fingerprint (§3.2) in its low
  32 bits, and which reads back the same value. Hosts ignore it; its job is to
  put the fingerprint somewhere the GATT **Database Hash** (`0x2B2A`) covers,
  since that hash includes characteristic *declarations* but not characteristic
  values — so a host doing robust caching notices any descriptor change,
  including one that alters the Report Map's value alone. See CONNECTION.md
  §7.2b, which also covers the on-demand `[f]` repair (Service Changed, `0x2A05`).

bluetoothd owns the GAP (`0x1800`) and Generic Attribute (`0x1801`) services and
builds Service Changed and the Database Hash itself; a D-Bus GATT application
cannot register them, only change the database they describe.

#### Advertising, security and sessions

- **Advertising:** advertisement type Peripheral, the HID service UUID
  (`0x1812`), local name `blooter`, Keyboard **Appearance** (`0x03C1`) — a combo
  keyboard/mouse device advertises the keyboard icon, matching the identity the
  classic transport sets as its Class of Device, and it stays `0x03C1` with the
  TV remote enabled too, since hosts key HID handling off the report map rather
  than the appearance (REMOTE.md §9) — and discoverable/connectable.
  This does **not** replace the Class-of-Device and adapter-name logic of
  `setup.rs`, which therefore runs in LE mode too. The advertisement only
  reaches a host *before* it connects; once connected the host reads bluetoothd's
  GAP service, whose Device Name is the adapter alias and whose Appearance is
  derived from the adapter's Class of Device. Leaving those alone makes a host
  show the machine's hostname and its computer icon. `[ble] advertise` decides
  how far blooter takes them over (CONNECTION.md §4.1). BlueZ's derivation is
  lossy — class `0x000540` reads back as appearance `0x0540`, not `0x03C1`, and
  there is no D-Bus way to set GAP Appearance directly — so the advertisement
  stays the only place the correct value appears.
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
- **Connection tracking:** a host is "connected" once it has *both* a link up
  (`Device.Connected`, watched on D-Bus) and a subscription to any Report
  characteristic's CCCD. Both halves are needed because neither implies the
  other: the link comes up well before the host subscribes, and a *bonded* host's
  subscription outlives its link — bluetoothd preserves the CCC state across a
  disconnect and restores it on reconnect, and so never calls `StopNotify`
  (`att_disconnected` in bluez's `src/gatt-database.c`). On connect blooter
  pushes initial zeroed reports so the host has state; `send_report` no-ops for
  any report the host has not subscribed to. The session ends when the link drops
  or the last subscription is dropped.
- **Initiating a connection: blooter does not, and cannot.** HOGP puts the HID
  device in the GAP Peripheral role, so only the host can open the link or start
  pairing; a connectable advertisement is blooter's whole half of reconnecting.
  `Device.Connect()` would also take the BR/EDR bearer on any dual-mode host,
  which is where `br-connection-unknown` came from. `[connection] reconnect` is
  Classic-only, and the LE menu manages bonded hosts rather than picking one to
  dial (CONNECTION.md §4, §6).
- **Out of scope (as for classic, TODO.md):** output reports (keyboard LEDs) and
  boot-protocol mode; more than one bonded host at a time; and advertising
  Classic and LE simultaneously as one logical device (the transport is chosen
  at launch).

## 5. Wire format of input reports

Every report is prefixed with the HIDP header byte `0xA1` (HIDP `DATA`
transaction, report type Input).

**Mouse report — 6 bytes (`axis_bits = 8`, the default):**

| Byte | Meaning |
|---|---|
| 0 | `0xA1` |
| 1 | Report ID = `1` |
| 2 | Buttons: bit 0 left, bit 1 right, bit 2 middle; bits 3–7 zero |
| 3 | X: relative motion, signed 8-bit (clamped to −127…127) |
| 4 | Y: relative motion, signed 8-bit |
| 5 | Wheel: relative scroll, signed 8-bit |

**Mouse report — 8 bytes (`axis_bits = 16`):** as above, but X and Y are signed
16-bit little-endian (clamped to −32767…32767) in bytes 3–4 and 5–6, with the
signed 8-bit Wheel in byte 7. This is the width at which accumulated motion
(§7.2c) never saturates.

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

**Consumer report — 4 bytes** (only with `[remote] enabled`, REMOTE.md §4):

| Byte | Meaning |
|---|---|
| 0 | `0xA1` |
| 1 | Report ID = `3 + n_gamepads` |
| 2–3 | The Consumer-page usage currently held, little-endian; `00 00` for none |

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
- Opened **read-write and non-blocking**, then read through `AsyncFd` like every
  other input source, as raw native-endian `struct input_event` records (24 bytes
  on 64-bit: `timeval` (16) + `u16 type` + `u16 code` + `s32 value`). A writer
  may split a record across writes, so bytes accumulate until one is whole.
  Treated as event device #0. Gamepad forwarding is disabled in FIFO mode.
- Opening read-write keeps a writer on the FIFO at all times, so an idle pipe
  reports "would block" instead of EOF; there is no reopen loop, and no blocking
  `open` waiting for a writer. This is a shutdown requirement, not a
  micro-optimisation — see §9.

### 6.3 Event draining

Events that arrive while **no host is connected** are still consumed (to keep
modifier/button state current for the hotkeys in §7.3) but not transmitted, as
transmission is gated on the connected state. Immediately before entering the
connected state, the pending-event channel is drained so stale keystrokes are
not delivered to the new host.

Accumulated pointer motion and the outgoing report buffer (§7.2c) are discarded
on the same principle: `InputState::reset` clears the accumulator on every new
connection, and dropping the session, exiting, or pausing capture clears both
*before* the all-keys-up/neutral reports are sent — those bypass the buffer, so
a report left queued behind them would re-latch state on the host.

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
`capture` flag, touchpad tracking state, the pending pointer accumulator and its
encoding (§7.2c), and one `GamepadState` per advertised slot. Reports are only
emitted while `capture` is true; state is tracked regardless, so hotkey chords
keep working with capture off.

### 7.1 `EV_KEY`

- **Mouse buttons** `BTN_LEFT`/`BTN_RIGHT`/`BTN_MIDDLE`: set/clear bit 0/1/2 of
  `mouse_buttons` and mark the pointer frame dirty. Like motion, the report is
  emitted at the frame boundary (§7.2c), so a click and the motion alongside it
  become one report and neither can overtake the other.
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

Relative motion, accumulated into the pending pointer frame (§7.2c) and emitted
at the next frame boundary — never per event, since a hardware frame carries one
event per axis and emitting per event would cost two reports for a plain
diagonal move:

- `REL_X` (0) → add value to the pending X.
- `REL_Y` (1) → add value to the pending Y.
- `REL_WHEEL` (8) → add value to the pending wheel.
- Accumulation is in `i32`; the clamp to the axis range happens only when a
  report is built. The button byte always reflects current `mouse_buttons`.

### 7.2b `EV_ABS` (touchpad)

Touchpads report absolute finger positions; blooter derives relative motion:

- Motion is ignored unless a finger is down (`BTN_TOUCH`).
- The first `ABS_X`/`ABS_Y` position after touch-down only seeds the reference
  (no jump on finger landing); subsequent positions add the per-axis delta to
  the pending pointer frame, exactly as §7.2 does. Lifting the finger resets the
  reference.

Touchpads are the highest event rate blooter sees, so this path is the main
beneficiary of §7.2c.

Gamepad `EV_ABS` events (sticks, triggers, D-pad) are routed by slot to §7.5.

### 7.2c Pointer batching

Pointer devices generate events far faster than a Bluetooth link consumes them.
The session loop sends serially — `Transport::send_report` is awaited inline —
so on a slow link motion queues up in the 256-slot event channel and the pointer
keeps gliding after the user stops. Batching replaces that queue with a bounded
accumulator, so lag is capped by the flush interval however fast events arrive.
Configured by the `[pointer]` table (§10.1).

**Frame boundaries.** `EV_SYN`/`SYN_REPORT` is translated to `Outcome::Sync` and
is the *only* point at which a pointer report is built. It is therefore
**necessary** for a flush in every mode — a timer or a full buffer arms a flush
rather than splitting a frame — and **sufficient** when `batch = "none"`. The
check precedes the gamepad slot dispatch, because every device shares one event
channel and a `SYN` from any of them is a valid boundary; a foreign `SYN` can
only flush *early*, never merge across frames.

**Accumulator.** `InputState` holds pending X/Y/wheel as `i32` plus a dirty flag
for button changes. `take_mouse_frame` hands out at most one report's worth,
clamped to the axis width and subtracted from the accumulator, returning `None`
once empty. `[pointer] overflow` decides how far a frame is drained:

- `"burst"` (default) — loop until empty: as many back-to-back reports as it
  takes. Lossless, bounded by the buffer.
- `"carry"` — one report; the remainder rides along on the next frame.
- `"clamp"` — one report; the remainder is discarded.

With `axis_bits = 16` (§3.2) the range is wide enough that this effectively
never comes into play.

**Outgoing buffer.** `transport::Outbox` is a fixed-capacity ring of built
reports, allocated once per connection (`[pointer] buffer`, default 16). Pushing
is a copy into a slot — no allocation, no spawned task, per the hot-path rule.
A push first tries to **merge into the tail entry**, and only when that entry is
a mouse report with an identical button byte and no axis would saturate. A
keyboard or gamepad tail blocks the merge, as does a button change, so entries
only ever fold backwards into their immediate predecessor and nothing can
overtake anything else. A full ring is flushed rather than dropped from.

**Flush drive** (`[pointer] batch`), evaluated at each frame boundary:

- `"auto"` — a per-transport minimum spacing via `Transport::flush_interval`:
  8 ms on Classic, 15 ms on BLE. The default, and the only mode that genuinely
  throttles BLE, because a GATT notification is handed to `bluetoothd` over
  D-Bus and returns long before the packet is on air.
- `"adaptive"` — no timer; flush every frame. The coalescing comes for free:
  while the previous send had the loop suspended, arriving events piled up in
  the channel and merged into the ring on the way through. Genuinely
  backpressure-driven on Classic, where the L2CAP socket buffer blocks;
  opportunistic only on BLE, for the reason above.
- `"none"` — flush every frame, with the accumulator still collapsing each frame
  to one report.
- `N` — an explicit millisecond spacing, overriding what `"auto"` would pick.

When no deadline is armed the timer branch of the session loop is
`future::pending()`, so an idle session runs no timer at all.

All other event types (`EV_MSC`, `EV_LED`, …) are ignored.

### 7.3 Local hotkeys and input capture

Hotkeys are configurable chords (§10); the built-in defaults are:

- **Left Ctrl, Left Alt, Right Shift** — terminate blooter cleanly (§9).
- **Left Shift, Right Shift** — toggle input capture. While capture is off, nothing
  is forwarded (an all-keys-up report and neutral gamepad reports are sent
  first so nothing stays latched host-side), and `-x` exclusive grabs are
  released so input reaches the local session again. A new host connection
  re-enables capture. `drop_connection` (drop the current host connection and
  return to accepting) and the separate `capture_on`/`capture_off` chords exist
  but are disabled by default.

**Order.** The **first** key a chord names must be pressed first — that is what
starts matching the chord — after which its remaining keys may be pressed in any
order. A chord fires the moment its last key goes **down**.

**The chord buffer.** A key that starts some chord is not forwarded when it is
pressed: it goes into the chord buffer, along with the chords it might still
complete. Further presses that keep at least one of those alive extend the
buffer, still forwarding nothing. Then either:

- **The chord completes** — the action fires and the whole buffer is consumed.
  Those keys are recorded as held: the host never saw them go down, so their
  autorepeats and eventual releases are swallowed too, and nothing can stay
  latched host-side.
- **No chord can follow** — an unrelated key goes down, or any of the buffered
  keys is released or repeats. The buffer is replayed: the keys held back are
  forwarded in press order, then the event that broke the chord, so the host
  sees the same sequence it would have without the delay. This is why
  translating one event can yield several reports (`report::Outcomes`).

So a key is only ever withheld while it might still be part of a chord, and only
from the moment it is pressed until the next key event. A key that starts no
chord — Right Shift under the defaults — is forwarded immediately, as usual.
Pointer motion, mouse buttons, touch and gamepad events never take part in
chords and leave a buffered prefix alone.

When several chords complete at once the longest wins (ties go to config order).
A chord whose keys are a subset of another's therefore shadows the longer one:
it fires first, so the longer chord can never be reached.

Dropping the connection first sends an all-keys-up keyboard report and neutral
gamepad reports.

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
| `KEY_SYSRQ` (PrintScreen), `KEY_SCROLLLOCK`, `KEY_PAUSE` | 70, 71, 72 |
| `KEY_INSERT`, `KEY_HOME`, `KEY_PAGEUP`, `KEY_DELETE`, `KEY_END`, `KEY_PAGEDOWN` | 73, 74, 75, 76, 77, 78 |
| `KEY_RIGHT`, `KEY_LEFT`, `KEY_DOWN`, `KEY_UP` | 79, 80, 81, 82 |
| `KEY_NUMLOCK`, `KEY_KPSLASH`, `KEY_KPASTERISK`, `KEY_KPMINUS`, `KEY_KPPLUS`, `KEY_KPENTER` | 83, 84, 85, 86, 87, 88 |
| `KEY_KP1` … `KEY_KP9`, `KEY_KP0`, `KEY_KPDOT` | 89 … 97, 98, 99 |

Everything else — media keys, `KEY_MENU`, `KEY_SEARCH`, … — is outside the usage
range 0–0x65 the keyboard collection declares. With `[remote]` off it is
unmapped and ignored; with `[remote] passthrough` on it goes to the *Consumer*
page instead, via `keymap::consumer_usage` (REMOTE.md §5), and reaches the host
as a consumer report rather than a keyboard one. A key taking part in a chord is
mapped no differently either way; whether it reaches the host is decided earlier,
by the chord buffer (§7.3).

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
  terminal); print `blooter stopped.`; then shut the runtime down with a short
  deadline.
- **Every input reader must be an async task, never a blocking one.**
  `JoinHandle::abort` does nothing to a `spawn_blocking` closure that is already
  running, and dropping a runtime waits for such threads without limit. FIFO mode
  used to read on a blocking thread, parked in a blocking `open` waiting for a
  writer: shutdown's abort was a no-op, and blooter printed `blooter stopped.`
  and then hung forever, restoring nothing (`BtSetup::drop` never ran, so the
  adapter kept blooter's class and name). §6.2 is what keeps that reader
  cancellable.
- The one blocking task that remains is the pairing prompt's stdin read
  (`agent::ask`), which cannot be made cancellable. `Runtime::shutdown_timeout`
  bounds it: an open prompt at exit costs a moment, not the process. By that
  point the adapter is restored and the grabs are released, so nothing is lost
  by detaching.

## 10. Configuration and adapter setup

### 10.1 Configuration file

A TOML file, looked up in order: the `-c<FILE>` path;
`$XDG_CONFIG_HOME/blooter/config.toml` (falling back to
`~/.config/blooter/config.toml`); `/etc/blooter/config.toml`. If none exists the
built-in defaults apply. `config.example.toml` documents every key with its
default value commented out.

- **`[hotkeys]`** — `drop_connection`, `exit`, `capture_toggle`, `capture_on`,
  `capture_off`. Each value is a chord: zero or more modifiers plus a final
  trigger key, joined with `+` (e.g. `"leftcontrol+leftalt+rightshift"`). The
  first key listed must be pressed first; the rest follow in any order, and the
  chord fires on the last keydown (§7.3). Key names follow
  [keyd](https://github.com/rvaiya/keyd) (`scrolllock`, `pause`, `f12`,
  `kpenter`, `leftmeta`, …); the side-agnostic aliases `control`/`ctrl`,
  `shift`, `alt`, `meta`/`super` match either side. `""` disables a hotkey.
- **`[gamepad] slots`** — `"initial"` (default), `0`, or a fixed count `N`
  (§6.4).
- **`[gamepad] hotplug`** — `"auto"` (default: on iff `slots` is a fixed count
  > 0), `"on"` (always monitor), or `"off"` (§6.4).
- **`[connection] protocol`** — `"ble"` (default, Bluetooth Low Energy / HOGP)
  or `"classic"` (BR/EDR HID). Selects the transport (§4).
- **`[connection] pairing`** — `"accept"` (default; silent "Just Works"),
  `"prompt_if_possible"` (prompt on the TTY when there is one, else accept) or
  `"prompt"` (always prompt; a startup error with no TTY) (CONNECTION.md §5).
- **`[connection] reconnect`** — an already-bonded host address
  `"AA:BB:CC:DD:EE:FF"` to initiate an outgoing connection to (CONNECTION.md
  §3.2 on Classic, §4 on BLE). Absent → accept-only unless the host menu
  supplies a target.
- **`[pointer] batch`** — `"auto"` (default: 8 ms on Classic, 15 ms on BLE),
  `"adaptive"`, `"none"`, or a millisecond count `N`. What drives a pointer
  flush (§7.2c).
- **`[pointer] buffer`** — outgoing report slots per connection, default `16`,
  minimum `1` (§7.2c).
- **`[pointer] axis_bits`** — `8` (default) or `16`: the width of the relative
  X/Y fields in the report descriptor (§3.2, §5). Changing it changes the
  descriptor, so bonded hosts must be fixed or re-paired (CONNECTION.md §7).
- **`[pointer] overflow`** — `"burst"` (default), `"carry"` or `"clamp"`: what
  happens when merged motion exceeds one report's range (§7.2c).
- **`[remote]`** — TV-remote emulation (REMOTE.md). `enabled` (default `false`)
  advertises the Consumer Control collection, which changes the descriptor, so
  bonded hosts must be fixed or re-paired (CONNECTION.md §7). `passthrough`
  (default `true`) forwards the local keyboard's media keys on the Consumer page
  (§7.4). Every other key in the table binds a remote button — a name such as
  `tv` or `channel_up`, or `"usage:0xNNN"` — to a chord in the `[hotkeys]`
  syntax, capped at `MAX_REMOTE_BINDINGS` (24). With `enabled = false` the
  bindings are parsed and then ignored, with a warning.

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
pairing`: silent accept (the default) or a TTY prompt (CONNECTION.md §5). In Classic mode the
adapter is also made **discoverable** (its prior state restored on exit), and
blooter prints that it is now visible, so a host can find and connect to it.

When stdin is a TTY, an interactive **host menu** runs **concurrently** with the
accept loop (CONNECTION.md §6): a short (~4 s) discovery pass, then a list of
known devices (connected first, then paired, then unpaired; each strongest-signal
first). The scan is filtered to the transport in use — BR/EDR inquiry on Classic,
LE scan on BLE — so the list only offers devices that can actually be connected
to. Selecting one pairs it from here if it is new, then the transport initiates
the outgoing connection to it: the HID L2CAP PSMs on Classic (CONNECTION.md
§3.2), `Device.Connect()` on BLE (§4). Because the menu is concurrent, an
**incoming** connection that arrives while it is open is taken as the user's
choice: blooter uses it and closes the menu with a note. Pressing Enter skips,
leaving blooter accepting (plus dialing any bonded `[connection] reconnect`
target). `[f] Fix connection` is offered on any bonded host, on either transport
(CONNECTION.md §7).
