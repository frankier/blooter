//! Command-line argument parsing. See design/ARCH.md §2.
//!
//! Supports both the original C attached forms (`-e3`, `-fmyfifo`) and the
//! separated forms (`-e 3`, `-f myfifo`) for script compatibility.

pub const USAGE: &str = "\
blooter — Bluetooth HID device emulator

Usage: blooter [OPTIONS]

Options:
  -h, -?, --help     Print this help and exit.
  -e<NUM>            Restrict input to /dev/input/event<NUM>. Repeatable.
                     Default: open every readable keyboard, mouse/trackpoint
                     and touchpad device.
  -f<NAME>           FIFO mode: read raw input_event records from FIFO <NAME>
                     (created 0600 if absent). Mutually exclusive with -e/-x.
  -l                 List available input devices and exit.
  -x                 Grab opened event devices exclusively (EVIOCGRAB).
  -c<FILE>           Read configuration from <FILE>. Default search path:
                     $XDG_CONFIG_HOME/blooter/config.toml (falling back to
                     ~/.config/blooter/config.toml), /etc/blooter/config.toml.
  -s, --skipsdp      Skip the D-Bus profile/SDP registration (debugging).
                     Classic only.
  -n, --nosetup      Skip the host (re)connection menu and, on Classic, adapter
                     setup (device class 0x0540, name \"blooter\", SSP pairing
                     mode). BLE needs the adapter for its GATT server, so only
                     the menu is skipped there.
  -d                 Enable debug logging of input events and socket traffic.

Local hotkeys (configurable, see config.example.toml):
Left Ctrl, Left Alt, Right Shift exits; Left Shift, Right Shift toggles
input capture on and off (press the first key of a chord first, the rest
in any order). Dropping the connection has no default chord.
";

#[derive(Debug, Default)]
pub struct Args {
    pub event_devices: Vec<u32>,
    pub fifo: Option<String>,
    pub config: Option<String>,
    pub list: bool,
    pub grab: bool,
    pub skipsdp: bool,
    pub nosetup: bool,
    pub debug: bool,
}

pub enum ParseResult {
    Run(Args),
    /// Print usage and exit 0.
    Help,
    /// Print the message and exit 1.
    Error(String),
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> ParseResult {
    let mut out = Args::default();
    let mut iter = args.into_iter().peekable();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "-?" | "--help" => return ParseResult::Help,
            "-s" | "--skipsdp" => out.skipsdp = true,
            "-n" | "--nosetup" => out.nosetup = true,
            "-l" => out.list = true,
            "-x" => out.grab = true,
            "-d" => out.debug = true,
            "-e" => match iter.next() {
                Some(v) => match v.parse::<u32>() {
                    Ok(n) => out.event_devices.push(n),
                    Err(_) => return ParseResult::Error(format!("Invalid argument: '-e {v}'")),
                },
                None => return ParseResult::Error("Invalid argument: '-e'".to_string()),
            },
            "-f" => match iter.next() {
                Some(v) => out.fifo = Some(v),
                None => return ParseResult::Error("Invalid argument: '-f'".to_string()),
            },
            "-c" | "--config" => match iter.next() {
                Some(v) => out.config = Some(v),
                None => return ParseResult::Error(format!("Invalid argument: '{arg}'")),
            },
            other if other.starts_with("-e") => {
                let rest = &other[2..];
                match rest.parse::<u32>() {
                    Ok(n) => out.event_devices.push(n),
                    Err(_) => return ParseResult::Error(format!("Invalid argument: '{other}'")),
                }
            }
            other if other.starts_with("-f") => {
                out.fifo = Some(other[2..].to_string());
            }
            other if other.starts_with("-c") => {
                out.config = Some(other[2..].to_string());
            }
            other => return ParseResult::Error(format!("Invalid argument: '{other}'")),
        }
    }

    ParseResult::Run(out)
}
