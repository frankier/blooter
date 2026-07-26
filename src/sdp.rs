//! The HID report descriptor and the SDP service-record XML published to
//! bluetoothd. See design/ARCH.md §3.1 and §3.2.

/// Report ID of the first gamepad; subsequent gamepads use 4, 5, … Mouse is
/// report ID 1 and keyboard is report ID 2 (see `BASE_DESCRIPTOR`).
pub const GAMEPAD_REPORT_ID_BASE: u8 = 3;

/// The 98-byte base HID report descriptor defining the mouse (report ID 1) and
/// keyboard (report ID 2) reports. The report-count for the mouse X/Y/Wheel
/// item is fixed to 3 here (see design/ARCH.md §3.2 quirk note); this does not change
/// the wire format. Gamepad collections (see `gamepad_block`) are appended
/// after this base by `report_descriptor`.
const BASE_DESCRIPTOR: [u8; 98] = [
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, //
    0x95, 0x03, 0x81, 0x02, 0x75, 0x05, 0x95, 0x01, 0x81, 0x01, 0x05, 0x01, //
    0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, //
    // Report Count for the three axes: fixed to 3 (original descriptor had 2).
    0x95, 0x03, 0x81, 0x06, 0xC0, 0xC0, 0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, //
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

/// The full HID report descriptor: the mouse/keyboard base plus one gamepad
/// collection per requested slot (report IDs `GAMEPAD_REPORT_ID_BASE`, …).
pub fn report_descriptor(n_gamepads: usize) -> Vec<u8> {
    let mut d = BASE_DESCRIPTOR.to_vec();
    for i in 0..n_gamepads {
        d.extend_from_slice(&gamepad_block(GAMEPAD_REPORT_ID_BASE + i as u8));
    }
    d
}

/// A short fingerprint of the report descriptor (FNV-1a). Hosts cache the
/// descriptor for the lifetime of the bond, so a change here is invisible to an
/// already-bonded host; comparing fingerprints identifies hosts holding a stale
/// copy (see `state::Hosts`, design/CONNECTION.md §7).
pub fn descriptor_fingerprint(n_gamepads: usize) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in report_descriptor(n_gamepads) {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// The HID Profile UUID.
pub const HID_UUID: &str = "00001124-0000-1000-8000-00805f9b34fb";

fn descriptor_hex(n_gamepads: usize) -> String {
    let desc = report_descriptor(n_gamepads);
    let mut s = String::with_capacity(desc.len() * 2);
    for b in desc {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Build the BlueZ record-XML `ServiceRecord` string (design/ARCH.md §3.1). The
/// embedded HID report descriptor advertises `n_gamepads` gamepad collections
/// in addition to the mouse and keyboard.
pub fn service_record_xml(n_gamepads: usize) -> String {
    let hex = descriptor_hex(n_gamepads);
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

    #[test]
    fn descriptor_length_is_dynamic() {
        // Base is the original 98-byte mouse+keyboard descriptor; each gamepad
        // appends one 85-byte collection.
        assert_eq!(report_descriptor(0).len(), 98);
        assert_eq!(report_descriptor(1).len(), 98 + 85);
        assert_eq!(report_descriptor(3).len(), 98 + 3 * 85);
        assert_eq!(descriptor_hex(0).len(), 196);
    }

    #[test]
    fn gamepad_blocks_carry_ascending_report_ids() {
        let desc = report_descriptor(2);
        // Report-ID items (0x85, id) for the two gamepads.
        assert!(desc.windows(2).any(|w| w == [0x85, GAMEPAD_REPORT_ID_BASE]));
        assert!(
            desc.windows(2)
                .any(|w| w == [0x85, GAMEPAD_REPORT_ID_BASE + 1])
        );
    }

    #[test]
    fn record_embeds_descriptor_hex() {
        let xml = service_record_xml(2);
        assert!(xml.contains(&descriptor_hex(2)));
        // Report-descriptor attribute and the HID report descriptor tag.
        assert!(xml.contains(r#"id="0x0206""#));
        assert!(xml.contains(r#"<uint8 value="0x22" />"#));
    }

    #[test]
    fn fingerprint_tracks_the_descriptor() {
        // Stable for a given slot count, distinct across counts — the whole
        // point is that adding/removing a gamepad is detectable.
        assert_eq!(descriptor_fingerprint(1), descriptor_fingerprint(1));
        let fps: Vec<u32> = (0..4).map(descriptor_fingerprint).collect();
        for (i, a) in fps.iter().enumerate() {
            for b in &fps[i + 1..] {
                assert_ne!(a, b, "fingerprints must differ per slot count");
            }
        }
    }

    #[test]
    fn record_dump_for_manual_validation() {
        // Emit to a file so it can be checked with an XML validator if desired.
        if let Ok(dir) = std::env::var("BLOOTER_DUMP_DIR") {
            std::fs::write(format!("{dir}/service_record.xml"), service_record_xml(2)).unwrap();
        }
    }
}
