# TV remote emulation (HID Consumer Control)

This document describes how blooter can present itself as a **Bluetooth TV
remote** in addition to the keyboard, mouse and gamepads it already emulates,
and what the limits of that are. It complements
[design/ARCH.md](ARCH.md) (§3.2 report descriptor, §5 wire formats, §7.4 keycode
tables) and [design/CONNECTION.md](CONNECTION.md) (§7 stale descriptors).

This is implemented: §8 lists the touch-points. The feature is off by default —
see §6 for the `[remote]` section that turns it on.

Two facts frame everything below:

- **There is no "Bluetooth remote control" profile.** Every BLE TV remote is a
  HID-over-GATT device — the same `0x1812` service blooter already serves —
  whose report map declares one extra top-level collection on the **Consumer**
  usage page. blooter is ~25 descriptor bytes and one report builder away from
  being one.
- **Input/source switching is not a HID function.** It belongs to HDMI-CEC.
  Volume, power, transport, navigation and "go to the TV tuner" are reachable;
  "switch to HDMI 2" is not. §2 makes this precise.

## 1. What a Bluetooth TV remote actually is

A commercial remote (Google TV, Fire TV, most smart-TV remotes) is a BLE
peripheral exposing:

- **HID service `0x1812`** — HID Information, Report Map, Protocol Mode, HID
  Control Point, and one Report characteristic per report ID, each with a
  Report Reference descriptor (`0x2908`) and a CCCD. This is exactly the tree
  blooter builds in `transport/le.rs` (ARCH.md §4.2).
- **Device Information `0x180A`** and **Battery `0x180F`** — blooter has both.
- A report map containing a **Keyboard collection** (for text entry in search
  boxes and D-pad navigation) and a **Consumer Control collection**:
  `Usage Page (Consumer) 0x0C`, `Usage (Consumer Control) 0x01`,
  `Collection (Application)`. The buttons are Consumer-page usages sent as
  input reports.

Beyond that, the only genuinely non-HID parts of a real remote are
vendor-specific: a proprietary GATT service carrying a voice-search audio
stream (Google's "Android TV Remote Service", Amazon's equivalent), and on some
models a pairing-assist service. Those are out of scope — blooter has no
microphone pipeline and voice search is not an input-forwarding feature.

The Classic (BR/EDR) path needs no special handling either: `transport/classic.rs`
writes report bytes verbatim onto the interrupt channel, so a consumer report is
just another `[0xA1, id, …]` frame.

## 2. What is and is not reachable

The two tables that decide this are external and authoritative:

- **`drivers/hid/hid-input.c`**, `case HID_UP_CONSUMER` — the kernel's Consumer
  usage → Linux keycode map. It defines what a BlueZ host sees.
- **`frameworks/base/data/keyboards/Generic.kl`** — Android's Linux keycode →
  `KeyEvent` map. It defines what an Android TV / Google TV host *does*.

A usage is only useful to blooter if it survives *both* hops. The set that does:

| Remote button | Consumer usage | Linux keycode | Android `KeyEvent` |
|---|---|---|---|
| Volume up / down | `0x0E9` / `0x0EA` | `KEY_VOLUMEUP` (115) / `KEY_VOLUMEDOWN` (114) | `VOLUME_UP` / `VOLUME_DOWN` |
| Mute | `0x0E2` | `KEY_MUTE` (113) | `VOLUME_MUTE` |
| Power | `0x030` | `KEY_POWER` (116) | `POWER` |
| Play/pause | `0x0CD` | `KEY_PLAYPAUSE` (164) | `MEDIA_PLAY_PAUSE` |
| Play (discrete) | `0x0B0` | `KEY_PLAY` (207) | `MEDIA_PLAY` |
| Next, previous | `0x0B5`, `0x0B6` | `KEY_NEXTSONG` (163), `KEY_PREVIOUSSONG` (165) | `MEDIA_NEXT`, `MEDIA_PREVIOUS` |
| Stop, record | `0x0B7`, `0x0B2` | `KEY_STOPCD` (166), `KEY_RECORD` (167) | `MEDIA_STOP`, `MEDIA_RECORD` |
| Fast-forward, rewind | `0x0B3`, `0x0B4` | `KEY_FASTFORWARD` (208), `KEY_REWIND` (168) | `MEDIA_FAST_FORWARD`, `MEDIA_REWIND` |
| Home | `0x223` (AC Home) | `KEY_HOMEPAGE` (172) | `HOME` |
| Back | `0x224` (AC Back) | `KEY_BACK` (158) | `BACK` |
| Menu | `0x040` | `KEY_MENU` (139) | `MENU` |
| Search | `0x221` (AC Search) | `KEY_SEARCH` (217) | `SEARCH` |
| Assistant | `0x1CB` | `KEY_ASSISTANT` (583) | `ASSIST` |
| Channel up / down | `0x09C` / `0x09D` | `KEY_CHANNELUP` (402) / `KEY_CHANNELDOWN` (403) | `CHANNEL_UP` / `CHANNEL_DOWN` |
| Last channel | `0x083` (Recall Last) | `KEY_LAST` (405) | `LAST_CHANNEL` |
| **Switch to TV** | `0x089` (Media Select TV) | `KEY_TV` (377) | `TV` |
| Guide | `0x08D` (Media Select Program Guide) | `KEY_PROGRAM` (362) | `GUIDE` |
| DVR / recordings | `0x09A` (Media Select Home) | `KEY_PVR` (366) | `DVR` |
| Captions / subtitles | `0x061` (Closed Caption) | `KEY_SUBTITLE` (370) | `CAPTIONS` |
| Info | `0x060` (Data On Screen) | `KEY_INFO` (358) | — (app-defined) |
| Red / green / yellow / blue | `0x069` / `0x06A` / `0x06B` / `0x06C` | `KEY_RED`…`KEY_BLUE` (398–401) | `PROG_RED`…`PROG_BLUE` |
| Aspect ratio | `0x06D` | `KEY_ASPECT_RATIO` | — |
| All apps | `0x2A2` | `KEY_ALL_APPLICATIONS` | `ALL_APPS` |

There is deliberately **no discrete pause** row. The Consumer page has `0x0B1`
Pause, but the kernel maps it to `KEY_PAUSE` (119) — the *Pause/Break* keyboard
key — and `Generic.kl` has no entry for 119, so it is dropped on Android TV.
Use `0x0CD` (play/pause toggle), which every host handles.

**Navigation (D-pad, OK, digits) should go through the Keyboard collection, not
the Consumer one.** The Consumer page has Menu Up/Down/Left/Right/Pick/Escape
(`0x042`–`0x046`, `0x041`), but the kernel maps them straight back onto
`KEY_UP`/`KEY_DOWN`/…/`KEY_SELECT`/`KEY_ESC`, so routing arrows through the
keyboard collection blooter already has is equivalent and simpler. The same
goes for digits and text entry — a real remote's on-screen keyboard is exactly
what blooter's keyboard collection replaces.

### 2.1 Source switching: not available over HID

There is no Consumer usage — no HID usage on any page — for "select HDMI 2" or
"cycle input". Input selection is an **HDMI-CEC** operation
(`<Set Stream Path>` / `<Device Menu Control>`), carried on the HDMI cable, not
over Bluetooth. A Bluetooth remote that switches your TV's input is either the
TV vendor's own remote speaking a vendor protocol to the TV directly, or a
source device (an Android TV box) issuing CEC on the user's behalf.

The nearest HID usage is **`0x082` Mode Step**, which the kernel maps to
`KEY_VIDEO_NEXT` (241). But `Generic.kl` has no entry for keycode 241 — there
is no `TV_INPUT` mapping anywhere in the file — so on Android TV it is dropped.
On a smart TV acting as the Bluetooth host directly, behaviour is
vendor-specific and untested.

**Decision:** expose `0x082` only through the raw-usage escape hatch (§6), never
under a friendly name like `input` or `source`, and document it as best-effort.
Promising a `source` binding that silently does nothing on the most common host
is worse than not offering it.

The useful consolation: on an Android TV / Fire TV box, **volume and power are
relayed to the TV over CEC by the box itself**. So a blooter remote does control
the television's volume and standby — just not its input selector.

### 2.2 Host behaviour beyond Android

- **BlueZ / Linux host** — `hid-input.c` is the whole story; every usage in the
  table above surfaces as the listed keycode on an `/dev/input/event*` node.
  Useful for testing: `evtest` on the host shows exactly what landed.
- **Windows** — handles Consumer Control usage arrays natively; volume,
  transport controls and browser keys work, the TV-specific usages do not.
- **macOS / Apple TV** — volume, mute and transport controls work; Apple TV
  accepts BLE HID keyboards and treats the keyboard collection as the primary
  navigation surface. The TV-tuner usages have no meaning there.

None of this needs per-host branching in blooter: the report map declares the
whole page range and the host maps what it understands.

## 3. Report descriptor

One new 25-byte application collection, appended **after** the gamepad
collections (§3.1 explains why last). In the style of ARCH.md §3.2:

```
05 0C           Usage Page (Consumer)
09 01           Usage (Consumer Control)
A1 01           Collection (Application)
85 <id>           Report ID (3 + n_gamepads)
15 00             Logical Minimum (0)
26 A2 02          Logical Maximum (0x02A2)
19 00             Usage Minimum (0x00)
2A A2 02          Usage Maximum (0x02A2)
75 10             Report Size (16)
95 01             Report Count (1)
81 00             Input (Data, Array, Absolute)
C0              End Collection
```

An **array** item, not a bitmap: the report carries the usage *code* of the
button currently held, so the collection covers the whole page without one
descriptor bit per button. Usage `0x00` is Unassigned and doubles as "nothing
held", the same trick the keyboard collection uses with `Usage Minimum (0)` in
its 8-byte key array. `0x02A2` is the highest usage blooter has any use for
(`AC Desktop Show All Applications`); declaring a range rather than enumerating
usages keeps the block a fixed 25 bytes regardless of how many buttons are
bound.

Report Count is **1**: one consumer button at a time. Remote buttons are not
chorded — nobody holds volume-up while pressing guide — and a count of 1 keeps
the session state to a single `u16`.

Descriptor totals with the collection enabled:

| Configuration | Bytes |
|---|---|
| 8-bit mouse + keyboard | 98 (unchanged) |
| … + consumer | 123 |
| 16-bit mouse + keyboard + consumer | 135 |
| … + one gamepad | +85 |

### 3.1 Report ID and placement

The consumer collection takes report ID **`3 + n_gamepads`** and is emitted
last, after every gamepad block.

This is what makes the feature free for users who do not enable it:
`GAMEPAD_REPORT_ID_BASE` stays `3`, gamepad IDs do not shift, and with
`[remote]` disabled `sdp::report_descriptor` produces **byte-identical output to
today** — so `descriptor_fingerprint` is unchanged and no existing bond is
disturbed. Putting the consumer collection at ID 3 and pushing gamepads to 4+
would have changed the descriptor for everyone.

The cost is a dynamic report ID, but gamepad IDs are already dynamic and
`transport/le.rs` builds its Report-characteristic list from a computed set of
IDs, so nothing downstream assumes a constant.

### 3.2 Effect on bonded hosts

Enabling `[remote]` changes the report descriptor, and hosts cache the
descriptor for the lifetime of the bond. Every already-bonded host therefore
keeps sending its old report map to the OS and will ignore consumer reports
until it re-reads it. This is precisely the situation CONNECTION.md §7 exists
for: the fingerprint recorded in `state::Hosts` no longer matches, the menu
marks the host stale, and `[f] Fix connection` (Service Changed + Database Hash
churn on BLE, virtual-cable unplug on Classic) forces a re-read. No new
machinery is needed — but the `[remote] enabled` docs must say that turning it
on requires a fix or a re-pair for existing hosts.

## 4. Wire format

After the `0xA1` HIDP prefix and the report-ID byte, the consumer report is
**2 bytes: the held usage, little-endian**, or `00 00` for "nothing held". With
no gamepads (report ID 3):

```
A1 03 E9 00     Volume Up pressed
A1 03 00 00     released
A1 03 89 00     Media Select TV pressed
A1 03 00 00     released
```

Total frame length 4 bytes, comfortably inside `report::MAX_REPORT` (11).

On the LE path, `transport/le.rs::send_report` strips the first two bytes and
notifies the 2-byte payload on the Report characteristic for that ID; on
Classic the whole frame goes out verbatim. Both are existing code paths.

## 5. Keycode → usage table (passthrough)

The passthrough half of the feature: the media keys a real keyboard already
emits, which today die at the `hid_usage` fall-through in `report::plain_key`.
This table is the inverse of `hid-input.c`'s consumer case, restricted to
keycodes evdev keyboards actually produce.

| Linux keycode | Consumer usage |
|---|---|
| `KEY_MUTE` (113), `KEY_VOLUMEDOWN` (114), `KEY_VOLUMEUP` (115) | `0x0E2`, `0x0EA`, `0x0E9` |
| `KEY_POWER` (116) | `0x030` |
| `KEY_STOP` (128) | `0x226` (AC Stop) |
| `KEY_MENU` (139) | `0x040` |
| `KEY_BACK` (158) | `0x224` |
| `KEY_FORWARD` (159) | `0x225` |
| `KEY_EJECTCD` (161) | `0x0B8` |
| `KEY_NEXTSONG` (163), `KEY_PLAYPAUSE` (164), `KEY_PREVIOUSSONG` (165) | `0x0B5`, `0x0CD`, `0x0B6` |
| `KEY_STOPCD` (166), `KEY_RECORD` (167), `KEY_REWIND` (168) | `0x0B7`, `0x0B2`, `0x0B4` |
| `KEY_HOMEPAGE` (172), `KEY_REFRESH` (173) | `0x223`, `0x227` |
| `KEY_MAIL` (155), `KEY_BOOKMARKS` (156) | `0x18A`, `0x22A` |
| `KEY_FILE` (144) | `0x194` |
| `KEY_AUDIO` (392) | `0x1B7` |
| `KEY_PLAY` (207) | `0x0B0` |
| `KEY_FASTFORWARD` (208) | `0x0B3` |
| `KEY_SEARCH` (217) | `0x221` |
| `KEY_BRIGHTNESSDOWN` (224), `KEY_BRIGHTNESSUP` (225) | `0x070`, `0x06F` |
| `KEY_WWW` (150) | `0x08A` |
| `KEY_CALC` (140) | `0x192` |
| `KEY_SCREENLOCK` / `KEY_COFFEE` (152) | `0x19E` |
| `KEY_ASSISTANT` (583) | `0x1CB` |
| `KEY_VOICECOMMAND` (582) | `0x0CF` |

Several kernel entries are many-to-one (`0x08A` and `0x196` both give
`KEY_WWW`; `0x182` and `0x22A` both give `KEY_BOOKMARKS`; `0x060` and `0x1BD`
both give `KEY_INFO`). The inverse picks the canonical one listed above; the
choice is invisible to the host because both map back to the same keycode.

The reverse also happens: `KEY_PLAYCD` (200) has no consumer usage of its own in
the kernel table, but Android treats it as `MEDIA_PLAY` just like `KEY_PLAY`
(207), so keyboards that emit it should map to `0x0B0` as well. `KEY_PAUSECD`
(201) has no usable target — see the note in §2.

Keycodes the kernel reaches only via the Consumer page but which no keyboard
emits (`KEY_TV`, `KEY_CHANNELUP`, `KEY_RED`, …) are deliberately **absent** from
this table — they are the virtual remote's job (§6), not passthrough's.

A key in this table is still subject to the chord buffer (ARCH.md §7.3) exactly
like any other: whether it reaches the host is decided before the usage lookup.

## 6. Configuration

A new `[remote]` section, absent by default:

```toml
[remote]
# Advertise the Consumer Control collection. Changing this changes the report
# descriptor, so already-bonded hosts need [f] Fix connection (CONNECTION.md §7).
enabled = true

# Forward the multimedia keys the local keyboard already emits (§5).
passthrough = true

# Chords bound to remote buttons the keyboard has no key for. Same syntax as
# [hotkeys]: the binding name is the key, the chord is the value, "" disables.
tv            = "leftmeta+t"
guide         = "leftmeta+g"
channel_up    = "leftmeta+pageup"
channel_down  = "leftmeta+pagedown"
last_channel  = "leftmeta+l"
captions      = "leftmeta+c"
dvr           = "leftmeta+d"
info          = "leftmeta+i"
red           = "leftmeta+f1"
green         = "leftmeta+f2"
yellow        = "leftmeta+f3"
blue          = "leftmeta+f4"
all_apps      = "leftmeta+a"
aspect_ratio  = "leftmeta+r"

# Escape hatch for anything unlisted, including best-effort usages such as
# Mode Step (§2.1), which does nothing on Android TV.
"usage:0x082" = "leftmeta+s"
```

- `enabled` defaults to **false**. With it false the descriptor is unchanged and
  the rest of the section is ignored (a warning, not an error, so a config can
  be prepared before flipping the switch).
- `passthrough` defaults to **true** when `enabled` is true. Setting it false
  gives a pure virtual remote — useful when the local keyboard's media keys
  should keep controlling the local machine.
- Binding names cover the buttons in §2 that have no keyboard equivalent. The
  chord values reuse `config::parse_chord_spec` unchanged, so the modifier
  aliases, the `MAX_CHORD_KEYS` limit and the ordered-first-step rule all carry
  over.
- `"usage:0xNNN"` accepts any usage in `0x000..=0x2A2`.

Chords are the right mechanism here rather than bare key rebinds: blooter's
premise is that every key is forwarded, so stealing a bare key from the host to
mean "channel up" would be a regression. A chord is invisible to the host until
it fires (ARCH.md §7.3).

**A fired `[remote]` chord is a tap**: it emits a press report and immediately a
release report. Holding the chord does not autorepeat. Channel-surfing therefore
means repeating the chord, which is acceptable for a keyboard-driven remote and
avoids entangling the chord state machine with key-repeat.

## 7. Translation and session state

`InputState` gains two fields: the usage currently reported (`u16`, `0` for
none) and the keycode that owns it (so a stale release cannot clear someone
else's press).

**Passthrough** (`report::plain_key`, after the `hid_usage` branch):

- **Press** (`value == 1`): look up `keymap::consumer_usage(code)`. Set the
  usage and owner, emit the report.
- **Release** (`value == 0`): only act if `code` is the current owner — clear to
  `0` and emit the zero report. Otherwise ignore. Two media keys held at once
  is last-press-wins, and releasing the loser must not cancel the winner.
- **Autorepeat** (`value == 2`): **ignored**. Re-sending an identical array
  report with no intervening zero is a no-op on most hosts, and hosts generate
  their own repeat via the input subsystem. This differs from the keyboard
  branch, which re-reports on autorepeat; the difference is deliberate.

**Virtual remote** (`report::apply_action`): a `Consumer(u16)` action pushes two
outcomes, press then release. `apply_action` therefore has to *push into* the
`Outcomes` buffer rather than return a single `Outcome`, and the buffer needs
one more slot than `MAX_CHORD_KEYS + 1`.

**Clearing.** A consumer zero report must accompany the existing
`keys_up_report` everywhere held state is dropped — capture-off, session drop,
and reset on a new host connection (ARCH.md §6.3). A host left believing a
consumer button is held is worse than a stuck keyboard key, because on a TV it
means a volume ramp that never stops.

**Ordering.** Consumer reports go through the same `Outbox` as everything else
and are not batched: unlike pointer motion they are discrete events, so they are
queued on the spot like keyboard reports.

## 8. Implementation touch-points

- **`sdp.rs`** — a `CONSUMER_BLOCK` builder taking the report ID (a function
  like `gamepad_block`, since the ID is dynamic), appended in
  `report_descriptor` after the gamepad loop when remote is enabled; a
  `consumer_report_id(n_gamepads)` helper beside `GAMEPAD_REPORT_ID_BASE`. The
  `n_gamepads`/`axis_bits` signature grows a third parameter, which
  `descriptor_fingerprint`, `descriptor_hex` and `service_record_xml` pass
  through. Existing length assertions in the module tests (98, 98 + 85, 110)
  stay valid for the disabled case; add enabled cases for 123 and 135.
- **`keymap.rs`** — the missing `KEY_*` constants; `consumer_usage(code) ->
  Option<u16>` (a separate function from `hid_usage`, which returns `u8`); the
  `[remote]` binding-name → usage table.
- **`report.rs`** — the two new `InputState` fields; `consumer_report()` and
  `consumer_up_report()` beside `keyboard_report`/`keys_up_report`; an
  `Outcome::Consumer` variant; the new branch in `plain_key`; `apply_action`
  converted to push into `Outcomes`; the `Outcomes` buffer sized
  `MAX_CHORD_KEYS + 2`.
- **`config.rs`** — `Action::Consumer(u16)`; a `Remote` struct on `Config`;
  parsing of the `[remote]` section reusing `parse_chord_spec` and `chord_item`.
  Note that `MAX_CHORDS` is currently `DEFAULTS.len()` and sizes the fixed
  `cands` array inside `report::ChordBuf`, which is `Copy` — it becomes
  `DEFAULTS.len() + MAX_REMOTE_BINDINGS`, so `MAX_REMOTE_BINDINGS` needs a cap
  (24 is ample: more buttons than a real remote has).
- **`transport/le.rs`** — add the consumer ID to the report-ID list that
  `hid_service` builds, so a Report characteristic and Report Reference
  descriptor exist for it. The GATT appearance stays `0x03C1` (Keyboard) — see
  §9. `transport/classic.rs` needs no change.
- **`main.rs`** — thread the remote config into `InputState` and into the
  descriptor calls.
- **Docs** — ARCH.md §3.2 (descriptor), §4.2 (Report characteristics), §5 (wire
  format) and §7.4, whose closing "Everything else (media keys, `KEY_MENU`, …)
  is unmapped/ignored" sentence becomes conditional and cross-references this
  file; `README.md`; `config.example.toml`. CONNECTION.md §7 gets a
  cross-reference for the bond-invalidation note in §3.2 above, not a restatement.

## 9. Alternatives considered

- **A bitmap report instead of a usage array.** One byte, eight named bits
  (vol±/mute/play/next/prev/home/back), no per-press state at all. Rejected: it
  caps the feature at eight buttons forever and makes the virtual remote — the
  half that reaches the TV-specific usages — impossible.
- **Report Count 2** (two simultaneous usages). Rejected: doubles the held-state
  bookkeeping to serve a case (holding two remote buttons) that does not occur.
- **Always-on collection, consumer at report ID 3.** Simpler code — a constant
  report ID and no config axis — but it changes the descriptor for every
  existing user, forcing all of them through `[f] Fix connection` for a feature
  most do not want. Rejected in favour of the gate in §6.
- **Changing the GATT appearance to `0x0180` (Remote Control) or `0x03CA`
  (HID / Presentation Remote).** Rejected: hosts key HID handling off the report
  map, not appearance; appearance only picks the icon, and blooter remains
  primarily a keyboard and mouse. `0x03C1` (Keyboard) stays correct.
- **Registering a second, separate HID device for the remote.** Rejected: BlueZ
  gives one HID service per adapter, blooter is explicitly single-adapter
  (TODO.md non-goals), and one report map with several collections is what real
  remotes do anyway.
- **Driving HDMI-CEC via `libcec` to get real input switching.** This is the
  only way to reach "switch source", but it is a different product: it needs a
  CEC adapter or a CEC-capable HDMI output on the blooter machine, has nothing
  to do with Bluetooth, and shares no code with the input pipeline. Out of
  scope; §2.1 states the limitation instead.

## 10. Testing

- **Unit** — descriptor lengths and well-formedness for the enabled cases;
  `consumer_usage` round-trips against a table of known pairs; fingerprint
  differs between enabled and disabled, and is *unchanged* from today when
  disabled (the regression that protects existing bonds); `[remote]` config
  parsing, including the `usage:0xNNN` form and the `MAX_REMOTE_BINDINGS` cap;
  translation cases for last-press-wins, stale release, autorepeat, and the
  zero report on capture-off.
- **btvirt** — extend the `LeHost` case to subscribe to the consumer Report
  CCCD and assert the notification bytes for a media key driven through the
  FIFO, and for a `[remote]` chord producing exactly two frames.
- **Manual** — against a BlueZ host, `evtest` on the far side confirms each
  usage in §2 arrives as the listed keycode. Against a real Android TV, confirm
  volume, play/pause, home, back and Media Select TV land, and confirm that Mode
  Step does not — that negative result is the one §2.1 rests on.
