//! The HID report descriptor and the SDP service-record XML published to
//! bluetoothd. See design/ARCH.md §3.1 and §3.2.

use crate::config::AxisBits;

/// Report ID of the first gamepad; subsequent gamepads use 4, 5, … Mouse is
/// report ID 1 and keyboard is report ID 2 (see `BASE_DESCRIPTOR`).
pub const GAMEPAD_REPORT_ID_BASE: u8 = 3;

/// What the HID report descriptor advertises: the gamepad slot count, the
/// pointer axis width and whether the Consumer Control ("TV remote") collection
/// is present. These three always travel together — a change to any of them is
/// a descriptor change an already-bonded host cannot see (design/CONNECTION.md
/// §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub n_gamepads: usize,
    pub axis_bits: AxisBits,
    /// `[remote] enabled` (design/REMOTE.md §3.1).
    pub remote: bool,
}

/// Report ID of the Consumer Control collection, which is emitted last so that
/// enabling it leaves every other report ID — and, when it is off, the whole
/// descriptor — byte-identical (design/REMOTE.md §3.1).
pub fn consumer_report_id(n_gamepads: usize) -> u8 {
    GAMEPAD_REPORT_ID_BASE + n_gamepads as u8
}

/// The mouse application collection (report ID 1) with 8-bit relative axes: 54
/// bytes, X/Y/Wheel in one Input item. The report-count for that item is fixed
/// to 3 here (see design/ARCH.md §3.2 quirk note); this does not change the wire
/// format. Input report is `[buttons, X, Y, Wheel]`, 4 bytes after the
/// `0xA1`/report-ID prefix.
const MOUSE_BLOCK_8: [u8; 54] = [
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, //
    0x95, 0x03, 0x81, 0x02, 0x75, 0x05, 0x95, 0x01, 0x81, 0x01, 0x05, 0x01, //
    0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, //
    // Report Count for the three axes: fixed to 3 (original descriptor had 2).
    0x95, 0x03, 0x81, 0x06, 0xC0, 0xC0,
];

/// The same collection with 16-bit relative X/Y (−32767..=32767) and an 8-bit
/// Wheel: 66 bytes. X/Y and Wheel need separate Input items because they differ
/// in Report Size and logical range, so the Wheel usage is declared *after* the
/// X/Y item that consumes the X and Y usages — the same two-item shape
/// `gamepad_block` uses for sticks and triggers. Input report is
/// `[buttons, X_lo, X_hi, Y_lo, Y_hi, Wheel]`, 6 bytes; the 5-bit padding after
/// the buttons keeps the 16-bit fields byte-aligned. See design/ARCH.md §3.2.
const MOUSE_BLOCK_16: [u8; 66] = [
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, //
    0x95, 0x03, 0x81, 0x02, 0x75, 0x05, 0x95, 0x01, 0x81, 0x01, 0x05, 0x01, //
    // X, Y: 16-bit relative, logical −32767..=32767.
    0x09, 0x30, 0x09, 0x31, 0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, //
    0x95, 0x02, 0x81, 0x06, //
    // Wheel: 8-bit relative, logical −127..=127.
    0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, //
    0xC0, 0xC0,
];

/// The keyboard application collection (report ID 2): 44 bytes. Input report is
/// `[modifiers, 8 key usages]`, 9 bytes after the `0xA1`/report-ID prefix.
const KEYBOARD_BLOCK: [u8; 44] = [
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, //
    0x85, 0x02, 0xA1, 0x00, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, //
    0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x08, 0x75, 0x08, //
    0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, //
    0xC0, 0xC0,
];

/// One standard Generic-Desktop Gamepad application collection with the given
/// report ID. Its input report (after the `0xA1`/report-ID HIDP prefix) is
/// `[buttons_lo, buttons_hi, hat, X, Y, Rx, Ry, Z, Rz]` — 16 buttons, a 4-bit
/// hat switch (+4-bit padding), the two sticks (X/Y, Rx/Ry) and the two
/// triggers (Z, Rz), all matching `report::GamepadState::report`.
fn gamepad_block(report_id: u8) -> [u8; 85] {
    [
        0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, report_id, //
        // 16 buttons.
        0x05, 0x09, 0x19, 0x01, 0x29, 0x10, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x10, 0x81,
        0x02, //
        // Hat switch (4 bits, null-capable) + 4-bit constant padding.
        0x05, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07, 0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14,
        0x75, 0x04, 0x95, 0x01, 0x81, 0x42, //
        0x65, 0x00, 0x75, 0x04, 0x95, 0x01, 0x81, 0x03, //
        // Sticks: X, Y, Rx, Ry (8-bit, 0..255).
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xFF, 0x00,
        0x75, 0x08, 0x95, 0x04, 0x81, 0x02, //
        // Triggers: Z, Rz (8-bit, 0..255).
        0x09, 0x32, 0x09, 0x35, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, //
        0xC0,
    ]
}

/// The Consumer Control application collection with the given report ID: 25
/// bytes. A single 16-bit *array* item carrying the usage code of the button
/// currently held (`0x0000` = nothing), so the whole page is covered without a
/// descriptor bit per button and the input report stays 2 bytes. Report Count
/// is 1: remote buttons are not chorded (design/REMOTE.md §3).
fn consumer_block(report_id: u8) -> [u8; 25] {
    [
        0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, report_id, //
        // Usage/logical range 0x0000..=0x02A2, the highest usage blooter binds.
        0x15, 0x00, 0x26, 0xA2, 0x02, 0x19, 0x00, 0x2A, 0xA2, 0x02, //
        // One 16-bit array element: Input (Data, Array, Absolute).
        0x75, 0x10, 0x95, 0x01, 0x81, 0x00, //
        0xC0,
    ]
}

/// The full HID report descriptor: the mouse collection (in the requested axis
/// width), the keyboard collection, one gamepad collection per requested slot
/// (report IDs `GAMEPAD_REPORT_ID_BASE`, …) and, with `[remote] enabled`, the
/// Consumer Control collection last (design/REMOTE.md §3.1).
pub fn report_descriptor(layout: Layout) -> Vec<u8> {
    let Layout {
        n_gamepads,
        axis_bits,
        remote,
    } = layout;
    let mouse: &[u8] = match axis_bits {
        AxisBits::Eight => &MOUSE_BLOCK_8,
        AxisBits::Sixteen => &MOUSE_BLOCK_16,
    };
    let mut d = Vec::with_capacity(mouse.len() + KEYBOARD_BLOCK.len() + n_gamepads * 85 + 25);
    d.extend_from_slice(mouse);
    d.extend_from_slice(&KEYBOARD_BLOCK);
    for i in 0..n_gamepads {
        d.extend_from_slice(&gamepad_block(GAMEPAD_REPORT_ID_BASE + i as u8));
    }
    if remote {
        d.extend_from_slice(&consumer_block(consumer_report_id(n_gamepads)));
    }
    d
}

/// A short fingerprint of the report descriptor (FNV-1a). Hosts cache the
/// descriptor for the lifetime of the bond, so a change here is invisible to an
/// already-bonded host; comparing fingerprints identifies hosts holding a stale
/// copy (see `state::Hosts`, design/CONNECTION.md §7).
pub fn descriptor_fingerprint(layout: Layout) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in report_descriptor(layout) {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// The HID Profile UUID.
pub const HID_UUID: &str = "00001124-0000-1000-8000-00805f9b34fb";

fn descriptor_hex(layout: Layout) -> String {
    let desc = report_descriptor(layout);
    let mut s = String::with_capacity(desc.len() * 2);
    for b in desc {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Build the BlueZ record-XML `ServiceRecord` string (design/ARCH.md §3.1), with
/// the report descriptor `layout` advertises embedded in it.
pub fn service_record_xml(layout: Layout) -> String {
    let hex = descriptor_hex(layout);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" ?>
<record>
  <attribute id="0x0001">
    <sequence>
      <uuid value="0x1124" />
    </sequence>
  </attribute>
  <attribute id="0x0004">
    <sequence>
      <sequence>
        <uuid value="0x0100" />
        <uint16 value="0x0011" />
      </sequence>
      <sequence>
        <uuid value="0x0011" />
      </sequence>
    </sequence>
  </attribute>
  <attribute id="0x0005">
    <sequence>
      <uuid value="0x1002" />
    </sequence>
  </attribute>
  <attribute id="0x0006">
    <sequence>
      <uint16 value="0x656e" />
      <uint16 value="0x006a" />
      <uint16 value="0x0100" />
    </sequence>
  </attribute>
  <attribute id="0x0009">
    <sequence>
      <sequence>
        <uuid value="0x1124" />
        <uint16 value="0x0100" />
      </sequence>
    </sequence>
  </attribute>
  <attribute id="0x000d">
    <sequence>
      <sequence>
        <sequence>
          <uuid value="0x0100" />
          <uint16 value="0x0013" />
        </sequence>
        <sequence>
          <uuid value="0x0011" />
        </sequence>
      </sequence>
    </sequence>
  </attribute>
  <attribute id="0x0100">
    <text value="Bluez virtual Mouse and Keyboard" />
  </attribute>
  <attribute id="0x0101">
    <text value="Keyboard" />
  </attribute>
  <attribute id="0x0102">
    <text value="blooter" />
  </attribute>
  <attribute id="0x0200">
    <uint16 value="0x0100" />
  </attribute>
  <attribute id="0x0201">
    <uint16 value="0x0111" />
  </attribute>
  <attribute id="0x0202">
    <uint8 value="0x40" />
  </attribute>
  <attribute id="0x0203">
    <uint8 value="0x00" />
  </attribute>
  <attribute id="0x0204">
    <boolean value="true" />
  </attribute>
  <attribute id="0x0205">
    <boolean value="true" />
  </attribute>
  <attribute id="0x0206">
    <sequence>
      <sequence>
        <uint8 value="0x22" />
        <text encoding="hex" value="{hex}" />
      </sequence>
    </sequence>
  </attribute>
  <attribute id="0x0207">
    <sequence>
      <sequence>
        <uint16 value="0x0409" />
        <uint16 value="0x0100" />
      </sequence>
    </sequence>
  </attribute>
  <attribute id="0x020b">
    <uint16 value="0x0100" />
  </attribute>
  <attribute id="0x020e">
    <boolean value="false" />
  </attribute>
</record>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const EIGHT: AxisBits = AxisBits::Eight;
    const SIXTEEN: AxisBits = AxisBits::Sixteen;

    /// A layout with `n` gamepad slots, the given axis width and the remote on
    /// or off — the three axes every descriptor test varies.
    fn layout(n: usize, axis_bits: AxisBits, remote: bool) -> Layout {
        Layout {
            n_gamepads: n,
            axis_bits,
            remote,
        }
    }

    #[test]
    fn descriptor_length_is_dynamic() {
        // Base is the 54-byte mouse plus the 44-byte keyboard collection; each
        // gamepad appends one 85-byte collection.
        assert_eq!(report_descriptor(layout(0, EIGHT, false)).len(), 98);
        assert_eq!(report_descriptor(layout(1, EIGHT, false)).len(), 98 + 85);
        assert_eq!(
            report_descriptor(layout(3, EIGHT, false)).len(),
            98 + 3 * 85
        );
        assert_eq!(descriptor_hex(layout(0, EIGHT, false)).len(), 196);
        // The 16-bit mouse collection is 12 bytes longer (a second Input item
        // for the wheel, and two-byte logical bounds for X/Y).
        assert_eq!(report_descriptor(layout(0, SIXTEEN, false)).len(), 110);
        assert_eq!(
            report_descriptor(layout(3, SIXTEEN, false)).len(),
            110 + 3 * 85
        );
        // The consumer collection adds a flat 25 bytes wherever it lands
        // (design/REMOTE.md §3).
        assert_eq!(report_descriptor(layout(0, EIGHT, true)).len(), 123);
        assert_eq!(report_descriptor(layout(0, SIXTEEN, true)).len(), 135);
        assert_eq!(report_descriptor(layout(1, EIGHT, true)).len(), 123 + 85);
    }

    /// The whole point of putting the consumer collection last: with `[remote]`
    /// off the descriptor is byte-identical to what it was before the feature
    /// existed, so no existing bond is disturbed (design/REMOTE.md §3.1).
    #[test]
    fn disabled_remote_leaves_the_descriptor_untouched() {
        for n in 0..4 {
            for bits in [EIGHT, SIXTEEN] {
                let plain = report_descriptor(layout(n, bits, false));
                let with_remote = report_descriptor(layout(n, bits, true));
                assert_eq!(
                    with_remote[..plain.len()],
                    plain[..],
                    "the consumer collection must only ever be appended"
                );
                // Gamepad report IDs do not shift; the consumer block takes the
                // next one after them.
                assert_eq!(consumer_report_id(n), GAMEPAD_REPORT_ID_BASE + n as u8);
                assert!(
                    with_remote[plain.len()..]
                        .windows(2)
                        .any(|w| w == [0x85, consumer_report_id(n)])
                );
            }
        }
    }

    /// The consumer block declares the Consumer page, an application collection
    /// and one 16-bit array element covering 0x0000..=0x02A2 (§3).
    #[test]
    fn consumer_block_is_well_formed() {
        let d = report_descriptor(layout(0, EIGHT, true));
        let at = d
            .windows(6)
            .position(|w| w == [0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01])
            .expect("Consumer Control application collection");
        assert_eq!(d[at + 6..at + 8], [0x85, consumer_report_id(0)]);
        // Logical/usage maxima are both 0x02A2, and the item is an array
        // (Input bit 1 clear), not a variable bitmap.
        assert_eq!(d[at + 10..at + 13], [0x26, 0xA2, 0x02]);
        assert_eq!(d[at + 15..at + 18], [0x2A, 0xA2, 0x02]);
        assert_eq!(d[at + 18..at + 24], [0x75, 0x10, 0x95, 0x01, 0x81, 0x00]);
        assert_eq!(d[at + 24], 0xC0);
        assert_eq!(at + 25, d.len(), "the collection must be emitted last");
    }

    /// The 16-bit variant must declare X/Y and Wheel as two separate Input
    /// items, with the Wheel usage after the item that consumes X and Y.
    #[test]
    fn sixteen_bit_block_is_well_formed() {
        let d = report_descriptor(layout(0, SIXTEEN, false));
        // Report Size 16, Report Count 2, Input(Data,Var,Rel) for X and Y.
        let xy = d
            .windows(6)
            .position(|w| w == [0x75, 0x10, 0x95, 0x02, 0x81, 0x06])
            .expect("16-bit X/Y input item");
        // Usage(X), Usage(Y) and the logical bounds precede it.
        assert_eq!(
            &d[xy - 10..xy],
            [0x09, 0x30, 0x09, 0x31, 0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F]
        );
        // Usage(Wheel) follows it, with its own 8-bit item.
        assert_eq!(
            &d[xy + 6..xy + 18],
            [
                0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06
            ]
        );
        // 3 button bits + 5 padding + 16 + 16 + 8 = 48 bits, byte-aligned.
        assert!(
            d.windows(6)
                .any(|w| w == [0x75, 0x05, 0x95, 0x01, 0x81, 0x01])
        );
    }

    #[test]
    fn gamepad_blocks_carry_ascending_report_ids() {
        let desc = report_descriptor(layout(2, EIGHT, false));
        // Report-ID items (0x85, id) for the two gamepads.
        assert!(desc.windows(2).any(|w| w == [0x85, GAMEPAD_REPORT_ID_BASE]));
        assert!(
            desc.windows(2)
                .any(|w| w == [0x85, GAMEPAD_REPORT_ID_BASE + 1])
        );
    }

    #[test]
    fn record_embeds_descriptor_hex() {
        let xml = service_record_xml(layout(2, EIGHT, false));
        assert!(xml.contains(&descriptor_hex(layout(2, EIGHT, false))));
        // Report-descriptor attribute and the HID report descriptor tag.
        assert!(xml.contains(r#"id="0x0206""#));
        assert!(xml.contains(r#"<uint8 value="0x22" />"#));
    }

    #[test]
    fn fingerprint_tracks_the_descriptor() {
        // Stable for a given slot count, distinct across counts — the whole
        // point is that adding/removing a gamepad is detectable.
        assert_eq!(
            descriptor_fingerprint(layout(1, EIGHT, false)),
            descriptor_fingerprint(layout(1, EIGHT, false))
        );
        let fps: Vec<u32> = (0..4)
            .map(|n| descriptor_fingerprint(layout(n, EIGHT, false)))
            .collect();
        for (i, a) in fps.iter().enumerate() {
            for b in &fps[i + 1..] {
                assert_ne!(a, b, "fingerprints must differ per slot count");
            }
        }
        // Switching axis width is a descriptor change too, so bonded hosts
        // holding the old layout get flagged (design/CONNECTION.md §7).
        for n in 0..4 {
            assert_ne!(
                descriptor_fingerprint(layout(n, EIGHT, false)),
                descriptor_fingerprint(layout(n, SIXTEEN, false)),
                "axis width must be visible in the fingerprint"
            );
            // As is enabling the remote, which is why turning it on needs
            // [f] Fix connection on already-bonded hosts (design/REMOTE.md §3.2).
            assert_ne!(
                descriptor_fingerprint(layout(n, EIGHT, false)),
                descriptor_fingerprint(layout(n, EIGHT, true)),
                "the consumer collection must be visible in the fingerprint"
            );
        }
    }

    /// The fingerprint with `[remote]` off is the one existing bonds were
    /// recorded under: pin the literal so a descriptor change cannot silently
    /// invalidate every stored host (design/REMOTE.md §3.1).
    #[test]
    fn disabled_fingerprint_is_unchanged() {
        assert_eq!(descriptor_fingerprint(layout(0, EIGHT, false)), 0x74bf_6be9);
    }

    #[test]
    fn record_dump_for_manual_validation() {
        // Emit to a file so it can be checked with an XML validator if desired.
        if let Ok(dir) = std::env::var("BLOOTER_DUMP_DIR") {
            std::fs::write(
                format!("{dir}/service_record.xml"),
                service_record_xml(layout(2, EIGHT, false)),
            )
            .unwrap();
        }
    }
}
