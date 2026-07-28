# blooter

A Bluetooth **HID device emulator** — it makes a Linux machine appear
to remote hosts as a Bluetooth keyboard + mouse (and, optionally, one or more
gamepads). It reads local Linux input events (`/dev/input/event*` or a FIFO),
translates them into Boot-style HID input reports, and forwards them to a
connected host over **Bluetooth Low Energy** (HID-over-GATT, the default) or
**Bluetooth Classic** (BR/EDR L2CAP), selected with `[connection] protocol`.

## Building

```
cargo build --release
```

The binary is `target/release/blooter`.

## Preconditions

- **Read access to the `/dev/input/event*` devices** you want to forward
  (typically root or membership in the `input` group).
- blooter registers a pairing agent and sets the adapter pairable (unless `-n`).
  The interactive host menu runs alongside accepting connections: pick a host to
  connect out to it, or just connect in from the other machine and the menu steps
  aside.

The **Classic** transport (`[connection] protocol = "classic"`) has two extra
requirements, which the default BLE transport does not:

- **`bluetoothd` must run with its own `input` plugin disabled**
  (`bluetoothd -P input`). Otherwise the HID UUID is already claimed by BlueZ
  and profile registration fails with `org.bluez.Error.NotPermitted`.
- **Binding the HID L2CAP PSMs (0x11 / 0x13) requires privilege.** These are
  below 0x1001, so you need `CAP_NET_BIND_SERVICE` or root:

  ```
  sudo setcap 'cap_net_bind_service=+ep' target/release/blooter
  ```

  or simply run as root.

For best host compatibility Classic also advertises a keyboard-like device class
on the adapter (e.g. `0x000540`), set during adapter setup along with making the
adapter discoverable (restored on exit). BLE advertises the equivalent Keyboard
*Appearance* in its LE advertisement instead, so it needs neither.

## Usage

```
blooter [-h|-?|--help] [-s|--skipsdp] [-n|--nosetup] [-e<NUM>]... [-f<NAME>] [-l] [-x] [-c<FILE>] [-d]
```

| Option | Behavior |
|---|---|
| `-h`, `-?`, `--help` | Print usage and exit. |
| `-e<NUM>` | Restrict input to `/dev/input/event<NUM>`. Repeatable; selections accumulate. Default: open every readable keyboard, mouse/trackpoint, touchpad and gamepad device (other event devices — power buttons, lid switches, etc. — are skipped). Both `-e3` and `-e 3` are accepted. |
| `-f<NAME>` | FIFO mode: read raw `struct input_event` records from a FIFO at `<NAME>` (created `0600` if absent). Mutually exclusive with `-e`/`-x`. |
| `-l` | List available input devices and exit. |
| `-x` | Grab opened event devices exclusively (`EVIOCGRAB`) so events do not reach the local session. Released on exit (and while input capture is toggled off). |
| `-c<FILE>` | Read the configuration from `<FILE>` instead of the default search path (see below). |
| `-s`, `--skipsdp` | Skip D-Bus profile/SDP registration (debugging only). Classic only. |
| `-n`, `--nosetup` | Skip the interactive host menu and, on Classic, adapter setup (device class, name, SSP). BLE needs the adapter for its GATT server, so only the menu is skipped there. |
| `-d` | Debug logging of input events and socket traffic. |

Logging verbosity can also be tuned with `RUST_LOG` (e.g. `RUST_LOG=debug`).

### Listing devices

```
$ blooter -l
List of available input devices:
num	Vendor/Product, Name, -x compatible (+/-), * = default scan
 3	[046d:c31c.0111] 'Logitech Keyboard' (+)*
```

The `(+)`/`(-)` column shows whether an exclusive grab (`-x`) succeeds, and a
trailing `*` marks devices the default scan (no `-e`) would pick up.
Devices you cannot read appear as `[permission denied]`.

### Local hotkeys

- **Left Ctrl, Left Alt, Right Shift** — terminate blooter cleanly.
- **Left Shift, Right Shift** — toggle input capture: while off, nothing is
  forwarded to the host (an all-keys-up report is sent first) and any `-x`
  exclusive grabs are released so input reaches the local session again.
- Dropping the current host connection (returning to accepting) has no default
  chord; bind `drop_connection` if you want one.

The key listed first has to be pressed first — Left Shift *then* Right Shift
toggles capture, the other way round does not — after which the rest of the
chord can be pressed in any order. The chord fires as soon as its last key goes
down, and none of its keys reach the host. Pressed on their own, those keys work
normally: Right Shift starts neither default chord, so it types capitals as
usual, and a Left Shift that turns out not to begin a chord is forwarded as soon
as the next key arrives.

All hotkeys are configurable — see below.

## Configuration

Hotkeys can be changed in a TOML config file, looked up in this order:

1. the file given with `-c<FILE>`;
2. `$XDG_CONFIG_HOME/blooter/config.toml`
   (falling back to `~/.config/blooter/config.toml`);
3. `/etc/blooter/config.toml`.

If none exists, the built-in defaults above apply. See
[`config.example.toml`](config.example.toml) for a full annotated example
whose (commented-out) values are exactly the defaults.

Each hotkey is a chord such as `"leftcontrol+leftalt+rightshift"`: zero or
more modifiers plus a final trigger key. The first key named must be pressed
first, the rest may follow in any order, and the chord fires on the last
keydown. Key names follow
[keyd](https://github.com/rvaiya/keyd)'s naming (`scrolllock`, `pause`,
`f12`, `kpenter`, `leftmeta`, …); the side-agnostic aliases
`control`/`ctrl`, `shift`, `alt` and `meta`/`super` match either side.
A chord's keys are consumed locally rather than forwarded — but only once they
are actually part of a chord being made, so a key that begins no chord, or one
whose chord turns out not to be coming, still reaches the host. When one chord's
keys are a subset of another's, the shorter one fires first and shadows the
longer. Setting a hotkey to `""` disables it.

Available keys: `drop_connection`, `exit`, `capture_toggle`, `capture_on`,
`capture_off` (the last two are disabled by default in favor of the toggle).

### Gamepads

Gamepads plugged in over USB are forwarded as HID game controllers **in addition
to** the keyboard and mouse. Each gamepad is exposed as its own controller (its
own HID report ID), so a host can distinguish several of them — you can forward
**more than one gamepad at a time**. Sticks, triggers, the D-pad and up to 16
buttons are forwarded.

The number of controllers is set by `[gamepad] slots` in the config file:

- `"initial"` (the default) — advertise one controller per gamepad present when
  blooter starts;
- `0` — disable gamepad forwarding entirely;
- `N` — advertise exactly `N` controllers; gamepads present at startup fill the
  slots in order and any extra slots stay idle.

Because the HID report descriptor is fixed when blooter registers with
`bluetoothd`, the *number* of advertised controllers cannot change while
running. The advertised slots can, however, be filled and refilled at runtime:
with a fixed `slots` count, `[gamepad] hotplug` uses a udev monitor to open a
gamepad plugged in after startup into a free slot, and frees that slot again
when the controller is unplugged.

- `"auto"` (the default) — hotplug when `slots` is a fixed count greater than
  zero (i.e. there are reserved slots to fill), otherwise off;
- `"on"` — always monitor (it simply finds no free slot when all are in use);
- `"off"` — never monitor.

With an explicit `-e` selection, only the listed event numbers are opened. This
needs system **libudev** (a build- and run-time dependency, present on any
systemd/udev Linux).

### Pointer batching

A mouse or touchpad produces events far faster than a Bluetooth link carries
them — a touchpad can emit hundreds of frames a second, while BLE delivers
roughly one notification per connection interval. Left alone, the surplus queues
up and the pointer keeps drifting for a moment after you stop moving. blooter
instead accumulates the motion between sends and delivers it as one larger
movement, which trades a little pointer resolution for responsiveness.

`[pointer] batch` sets what triggers a send:

- `"auto"` (the default) — leave at least one transport-appropriate interval
  between pointer reports: 8 ms on Classic, 15 ms on BLE;
- `"adaptive"` — no timer; merge only what arrives while the previous report is
  being sent. No added latency, and self-tuning on Classic — but on BLE a
  notification is handed to `bluetoothd` rather than paced by the radio, so
  there it cannot see the real link speed. Prefer `"auto"` on BLE;
- `"none"` — send at every input frame;
- `N` — leave at least `N` milliseconds, overriding `"auto"`.

An input frame is never split, in any mode, so a click and the motion alongside
it always arrive together.

A single report carries at most ±127 counts per axis, which a fast flick can
exceed once motion is merged. By default (`[pointer] overflow = "burst"`) the
excess goes out as extra reports, so nothing is lost; `"carry"` spreads it over
following reports instead, and `"clamp"` drops it. Setting
`[pointer] axis_bits = 16` widens the axes to ±32767 so this stops arising at
all — but it changes the HID report descriptor, which hosts cache, so already
paired hosts need fixing (`[f]` in the connection menu).

## How it works

On **BLE** (the default):

1. Registers a GATT server with `bluetoothd` — the HID service (`0x1812`) with
   one Report characteristic per report id, plus Device Information and Battery —
   and an LE advertisement of that service, named `blooter` with the Keyboard
   appearance.
2. A host is connected once it subscribes to a Report characteristic's CCCD, and
   disconnected when the last subscription drops.
3. Reads Linux input events, translates them into HID Boot mouse (report ID 1)
   and keyboard (report ID 2) reports, plus one gamepad report (report IDs 3,
   4, …) per forwarded controller, and notifies them on the matching
   characteristic.

On **Classic**:

1. Registers an HID profile (UUID `00001124-…`) with `bluetoothd` over D-Bus,
   publishing an SDP record so hosts discover the machine as a keyboard/mouse
   (plus any gamepad collections). Any connection `bluetoothd` routes to the
   profile is ignored — real traffic is handled on our own listeners.
2. Listens on the two standardized HID L2CAP PSMs — **0x11 (control)** and
   **0x13 (interrupt)** — accepting one host at a time. After a host connects
   on the control PSM, blooter waits up to 3 s for the interrupt PSM.
3. Translates input events into the same reports and sends them on the interrupt
   channel.

Pointer input is relative motion (mouse/trackpoint, and touchpads mapped to
relative motion); the keyboard covers the standard Boot usage range. There is no
keyboard-LED output handling. On either transport blooter can also **initiate**
the connection to an already-bonded host when a target is set via the host menu
or `[connection] reconnect`; otherwise it only accepts. Pairing is handled by a
built-in agent (`[connection] pairing`). See
[design/CONNECTION.md](design/CONNECTION.md).

### Fixing a host that ignores a layout change

Hosts cache blooter's HID descriptor for the lifetime of their bond, so changing
the number of advertised gamepads (or the pointer axis width) has **no effect on
an already-paired host** — it keeps using the layout it cached, and the new
controller silently never shows up. blooter warns about such hosts at startup and
marks them `stale` in the connection menu; select one and press **`[f]`** to fix
it.

What that does depends on the transport. On **BLE** it connects to the host and
changes blooter's GATT database under it, which makes bluetoothd tell the host
its cached copy is stale (a Service Changed indication); the host re-reads the
layout by itself, with no re-pairing. Many hosts pick the change up on their own
at the next connection, without `[f]`. On **Classic** it tells the host to forget
blooter (an HID virtual-cable unplug) and drops the bond on both sides, so
re-pairing from that host picks up the current layout.

Either way, a host that cannot be reached (or that ignores the notification) has
to be repaired by hand: remove blooter from its Bluetooth settings and pair
again. Setting a fixed `[gamepad] slots = N` avoids the situation entirely. See
[design/CONNECTION.md](design/CONNECTION.md) §7.

## Exit codes

`0` on success; nonzero with a clear message on any startup failure
(argument/registration errors, input-open failure, socket bind failure).
