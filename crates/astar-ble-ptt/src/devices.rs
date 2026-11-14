// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The device table: which Bluetooth LE buttons astar recognises, how a real
//! one is told apart from a lookalike, and how its bytes are read.
//!
//! The table is closed on purpose. Nobody can type in a UUID or an encoding,
//! because a mistyped characteristic on some other device must never become a
//! keying source. Adding a device takes a listen-only probe, one row here, a
//! decode test from the captured bytes, a scenario trace and a docs row.
//!
//! Recorded so nobody retries them on the B02PTT: Battery (`180F`/`2A19`) is
//! advertised but not discoverable on the unpaired LE link, and the GAIA,
//! `6666` and `00000007-7770` services, classic SPP and AVRCP carry no button.

use core::fmt;

/// A 128-bit GATT UUID, held in the byte order it is printed in.
///
/// The crate has no dependencies, so it carries its own sixteen-byte type
/// rather than pulling in a UUID crate for one parser and one formatter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Uuid128(pub [u8; 16]);

/// Builds a [`Uuid128`] from a string literal at compile time, so a typo in a
/// table row fails the build instead of shipping a row that matches nothing.
#[macro_export]
macro_rules! uuid128 {
    ($s:literal) => {
        $crate::devices::Uuid128::parse_const($s)
    };
}

/// The value of one ASCII hex digit, either case.
const fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Parses `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`. Shared by the compile-time
/// and the runtime parser so the two can never disagree.
const fn parse_hyphenated(s: &[u8]) -> Option<[u8; 16]> {
    if s.len() != 36 {
        return None;
    }
    let mut out = [0u8; 16];
    let mut i = 0;
    let mut n = 0;
    while i < 36 {
        if i == 8 || i == 13 || i == 18 || i == 23 {
            if s[i] != b'-' {
                return None;
            }
            i += 1;
        } else {
            let (Some(hi), Some(lo)) = (hex_value(s[i]), hex_value(s[i + 1])) else {
                return None;
            };
            out[n] = hi * 16 + lo;
            n += 1;
            i += 2;
        }
    }
    Some(out)
}

impl Uuid128 {
    /// Parses `"XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX"`, either case.
    ///
    /// It is `const` so a bad table row is a build error, not a runtime
    /// surprise: in a `const` or `static` initialiser a malformed string stops
    /// compilation. Strings that arrive at runtime go through
    /// [`Uuid128::parse`], which never panics.
    #[must_use]
    pub const fn parse_const(s: &str) -> Uuid128 {
        if let Some(bytes) = parse_hyphenated(s.as_bytes()) {
            Uuid128(bytes)
        } else {
            panic!("not a 36-character hyphenated UUID")
        }
    }

    /// Parses a UUID that arrived at runtime (over the C ABI, from
    /// `CoreBluetooth`), either case. `None` when it is malformed.
    #[must_use]
    pub fn parse(s: &str) -> Option<Uuid128> {
        parse_hyphenated(s.as_bytes()).map(Uuid128)
    }
}

/// Uppercase and hyphenated, 36 characters: the form `CoreBluetooth` prints,
/// so a UUID that crosses the C ABI compares equal as a string.
impl fmt::Display for Uuid128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

/// The GATT characteristic property bit for NOTIFY. The button reports its
/// state only by notification, so a characteristic without it cannot be the
/// button, whatever its UUID.
pub const NOTIFY: u8 = 0x10;

/// How a device's notify payload maps to the button's state. A closed enum,
/// because an encoding is code reviewed against a captured trace, never
/// configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decode {
    /// The payload is exactly one byte: `press` while held, `release` when let go.
    SingleByte {
        /// The byte the device sends when the button goes down.
        press: u8,
        /// The byte the device sends when the button comes up.
        release: u8,
    },
}

/// How the client checks that the link is alive while the button holds the key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Liveness {
    /// No probe; the LE supervision timeout and the station's lease are the bound.
    None,
    /// `CBPeripheral.readRSSI`: a controller-level query that sends nothing to
    /// the device itself, so it cannot break "nothing writes to the device".
    Rssi,
}

/// Whether the device also carries audio, so the client knows to offer it as
/// the config's mic and speaker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceKind {
    /// A speaker-mic: the button plus classic HFP audio.
    SpeakerMic,
    /// A button and nothing else.
    ButtonOnly,
}

/// One notify payload read through a device's [`Decode`] rule. Every payload
/// is exactly one of these, so an unknown value can never key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decoded {
    /// The button went down.
    Press,
    /// The button came up.
    Release,
    /// Anything else. The machine treats it as a release and latches.
    Malformed,
}

/// One characteristic as service discovery reported it: what [`accepts_gatt`]
/// inspects after connecting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GattCharacteristic {
    /// The characteristic's UUID.
    pub uuid: Uuid128,
    /// Its GATT property bits; [`NOTIFY`] must be among them.
    pub properties: u8,
}

/// One supported button model.
#[derive(Debug, PartialEq, Eq)]
pub struct BleButtonDevice {
    /// Stable key stored in configs. Never shown.
    pub id: &'static str,
    /// What the UI calls it.
    pub display_name: &'static str,
    /// Advertised-name prefix. The per-unit suffix (`-6522`) is ignored.
    pub name_prefix: &'static str,
    /// The GATT service that holds the button.
    pub service: Uuid128,
    /// The button's characteristic. It must have NOTIFY.
    pub characteristic: Uuid128,
    /// How its payload reads.
    pub decode: Decode,
    /// How the link is probed while keyed.
    pub liveness: Liveness,
    /// Speaker-mic (offer the audio helper) or button only.
    pub kind: DeviceKind,
    /// A self-contained label for the listen-only probe that established this
    /// row: which unit, and when. It crosses the C ABI, so it names no file.
    pub verified: &'static str,
}

/// Every button astar will key from. Each row is backed by a listen-only
/// probe, and nothing outside this table can become a keying source.
pub static DEVICES: &[BleButtonDevice] = &[BleButtonDevice {
    id: "inrico-b02ptt",
    display_name: "Inrico B02PTT",
    name_prefix: "B02PTT",
    service: uuid128!("89A8591D-BB19-485B-9F59-58492BC33E24"),
    characteristic: uuid128!("894C8042-E841-461C-A5C9-5A73D25DB08E"),
    decode: Decode::SingleByte {
        press: 0x01,
        release: 0x00,
    },
    liveness: Liveness::Rssi,
    kind: DeviceKind::SpeakerMic,
    verified: "Inrico B02PTT GATT probe, 2026-10-08",
}];

/// The table row whose `name_prefix` starts the advertised name.
///
/// Case-sensitive, because the vendor's name is. A peer that advertises no
/// name never matches. A match only makes a peer a candidate: [`accepts_gatt`]
/// and the one-time confirm press still decide whether it is the button.
#[must_use]
pub fn match_advertised_name(name: Option<&str>) -> Option<&'static BleButtonDevice> {
    let name = name?;
    DEVICES
        .iter()
        .find(|device| name.starts_with(device.name_prefix))
}

/// True only when the connected peer's `service` is the device's service and
/// `characteristics` holds the device's characteristic with NOTIFY.
///
/// This is what rejects a lookalike: a peer named like the button that does
/// not expose the button's service is disconnected and never subscribed.
#[must_use]
pub fn accepts_gatt(
    device: &BleButtonDevice,
    service: Uuid128,
    characteristics: &[GattCharacteristic],
) -> bool {
    service == device.service
        && characteristics
            .iter()
            .any(|c| c.uuid == device.characteristic && (c.properties & NOTIFY) != 0)
}

/// Reads one payload. Exactly `[press]` is Press, exactly `[release]` is
/// Release, and anything else, including an empty or a longer payload, is
/// Malformed.
#[must_use]
pub fn decode(rule: Decode, payload: &[u8]) -> Decoded {
    let Decode::SingleByte { press, release } = rule;
    match payload {
        [byte] if *byte == press => Decoded::Press,
        [byte] if *byte == release => Decoded::Release,
        _ => Decoded::Malformed,
    }
}
