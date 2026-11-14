// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The device table, advertised-name matching, GATT acceptance and decode.
//!
//! The UUID strings below are restated from the listen-only probe of the
//! B02PTT, not read from the table, so a typo in the table fails here.

use std::collections::HashSet;

use astar_ble_ptt::{
    BleButtonDevice, DEVICES, Decode, Decoded, DeviceKind, GattCharacteristic, Liveness, NOTIFY,
    NotifyBytes, Uuid128, accepts_gatt, decode, match_advertised_name,
};

const SERVICE: &str = "89A8591D-BB19-485B-9F59-58492BC33E24";
const CHARACTERISTIC: &str = "894C8042-E841-461C-A5C9-5A73D25DB08E";
/// Other LE services the B02PTT exposes (from the probe). None is the button.
const GAIA: &str = "00001100-D102-11E1-9B23-00025B00A5A5";
const SERVICE_6666: &str = "00006666-0000-1000-8000-00805F9B34FB";
const SERVICE_7770: &str = "00000007-7770-1000-8000-00805F9B34FB";
const BATTERY_LEVEL: &str = "00002A19-0000-1000-8000-00805F9B34FB";

fn uuid(s: &str) -> Uuid128 {
    Uuid128::parse(s).expect("test UUID parses")
}

fn ch(id: &str, properties: u8) -> GattCharacteristic {
    GattCharacteristic {
        uuid: uuid(id),
        properties,
    }
}

fn b02() -> &'static BleButtonDevice {
    &DEVICES[0]
}

#[test]
fn table_has_exactly_the_b02ptt_row() {
    assert_eq!(DEVICES.len(), 1);
    assert_eq!(
        DEVICES[0],
        BleButtonDevice {
            id: "inrico-b02ptt",
            display_name: "Inrico B02PTT",
            name_prefix: "B02PTT",
            service: uuid(SERVICE),
            characteristic: uuid(CHARACTERISTIC),
            decode: Decode::SingleByte {
                press: 0x01,
                release: 0x00
            },
            liveness: Liveness::Rssi,
            kind: DeviceKind::SpeakerMic,
            verified: "Inrico B02PTT GATT probe, 2026-10-08",
        }
    );
}

#[test]
fn ids_are_unique_and_press_differs_from_release() {
    let ids: HashSet<&str> = DEVICES.iter().map(|d| d.id).collect();
    assert_eq!(ids.len(), DEVICES.len());
    for device in DEVICES {
        let Decode::SingleByte { press, release } = device.decode;
        assert_ne!(press, release, "{}", device.id);
    }
}

#[test]
fn notify_bit_is_the_gatt_notify_property() {
    assert_eq!(NOTIFY, 0x10);
}

#[test]
fn uuid_display_is_uppercase_hyphenated() {
    assert_eq!(b02().service.to_string(), SERVICE);
    assert_eq!(b02().characteristic.to_string(), CHARACTERISTIC);
    assert_eq!(
        Uuid128([0; 16]).to_string(),
        "00000000-0000-0000-0000-000000000000"
    );
}

#[test]
fn uuid_bytes_are_in_print_order() {
    let digits = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD,
        0xEF,
    ];
    assert_eq!(
        Uuid128::parse("01234567-89ab-cdef-0123-456789ABCDEF"),
        Some(Uuid128(digits))
    );
    assert_eq!(
        Uuid128(digits).to_string(),
        "01234567-89AB-CDEF-0123-456789ABCDEF"
    );
    assert_eq!(
        Uuid128::parse("00112233-4455-6677-8899-aabbccddeeff"),
        Some(Uuid128([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF,
        ]))
    );
}

#[test]
fn uuid_parse_accepts_either_case() {
    assert_eq!(Uuid128::parse(&SERVICE.to_lowercase()), Some(b02().service));
    assert_eq!(Uuid128::parse(SERVICE), Some(b02().service));
}

#[test]
fn uuid_const_parse_and_macro_match_runtime_parse() {
    const BUILT: Uuid128 = astar_ble_ptt::uuid128!("894C8042-E841-461C-A5C9-5A73D25DB08E");
    assert_eq!(BUILT, b02().characteristic);
    assert_eq!(Uuid128::parse_const(SERVICE), uuid(SERVICE));
}

#[test]
fn uuid_parse_rejects_malformed() {
    for bad in [
        "",
        "89A8591D-BB19-485B-9F59-58492BC33E2",
        "89A8591D-BB19-485B-9F59-58492BC33E244",
        "89A8591DXBB19-485B-9F59-58492BC33E24",
        "89A8591D-BB19-485B-9F59-58492BC33E2G",
        "89A8591DBB19-485B-9F59-58492BC33E24-",
        "89A8591D-BB19-485B-9F5958492BC33E24-",
        "89A8591D-BB19-485B-9F59-58492BC33Eé",
        "{89A8591D-BB19-485B-9F59-58492BC33E}",
    ] {
        assert_eq!(Uuid128::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn advertised_name_matches_the_case_sensitive_prefix() {
    for good in ["B02PTT-6522", "B02PTT"] {
        let found = match_advertised_name(Some(good)).expect("matches");
        assert!(std::ptr::eq(found, &raw const DEVICES[0]), "{good}");
    }
    for bad in [
        Some("b02ptt-1"),
        Some("xB02PTT"),
        Some("B02PT"),
        Some(""),
        None,
    ] {
        assert_eq!(match_advertised_name(bad), None, "{bad:?}");
    }
}

#[test]
fn accepts_gatt_requires_service_characteristic_and_notify() {
    let d = b02();
    assert!(accepts_gatt(d, d.service, &[ch(CHARACTERISTIC, NOTIFY)]));
    assert!(accepts_gatt(
        d,
        d.service,
        &[ch(BATTERY_LEVEL, NOTIFY), ch(CHARACTERISTIC, NOTIFY | 0x02)]
    ));
    // Without NOTIFY: read-only, indicate-only, no properties at all.
    for properties in [0x02, 0x20, 0x00] {
        assert!(
            !accepts_gatt(d, d.service, &[ch(CHARACTERISTIC, properties)]),
            "{properties:#04x}"
        );
    }
    // Wrong service, wrong characteristic, nothing discovered.
    assert!(!accepts_gatt(d, uuid(GAIA), &[ch(CHARACTERISTIC, NOTIFY)]));
    assert!(!accepts_gatt(d, d.service, &[ch(BATTERY_LEVEL, NOTIFY)]));
    assert!(!accepts_gatt(d, d.service, &[]));
}

#[test]
fn lookalike_without_service_rejected() {
    // Named like the button, but its GATT holds only the services the probe
    // ruled out. The name makes it a candidate; GATT acceptance still says no.
    assert!(match_advertised_name(Some("B02PTT-9999")).is_some());
    for service in [GAIA, SERVICE_6666, SERVICE_7770] {
        assert!(
            !accepts_gatt(b02(), uuid(service), &[ch(CHARACTERISTIC, NOTIFY)]),
            "{service}"
        );
    }
}

#[test]
fn decode_is_total() {
    let rule = b02().decode;
    assert_eq!(decode(rule, &[0x01]), Decoded::Press);
    assert_eq!(decode(rule, &[0x00]), Decoded::Release);
    for payload in [
        &[][..],
        &[0x02][..],
        &[0x01, 0x00][..],
        &[0x01; 9][..],
        &[0x00, 0x00][..],
        &[0xFF][..],
    ] {
        assert_eq!(decode(rule, payload), Decoded::Malformed, "{payload:?}");
    }
    // A different rule is honoured on its own bytes, not the B02PTT's.
    let other = Decode::SingleByte {
        press: 0x7F,
        release: 0x80,
    };
    assert_eq!(decode(other, &[0x7F]), Decoded::Press);
    assert_eq!(decode(other, &[0x80]), Decoded::Release);
    assert_eq!(decode(other, &[0x01]), Decoded::Malformed);
    assert_eq!(decode(other, &[0x00]), Decoded::Malformed);
}

#[test]
fn notify_bytes_keep_the_true_length() {
    let rule = b02().decode;
    let one = NotifyBytes::from_slice(&[0x01]);
    assert_eq!(
        one,
        NotifyBytes {
            len: 1,
            bytes: [0x01, 0, 0, 0, 0, 0, 0, 0]
        }
    );
    assert_eq!(one.decoded(rule), Decoded::Press);
    assert_eq!(
        NotifyBytes::from_slice(&[0x00]).decoded(rule),
        Decoded::Release
    );

    let nine = NotifyBytes::from_slice(&[0x01; 9]);
    assert_eq!(nine.len, 9);
    assert_eq!(nine.bytes, [0x01; 8]);
    assert_eq!(nine.decoded(rule), Decoded::Malformed);

    let empty = NotifyBytes::from_slice(&[]);
    assert_eq!(
        empty,
        NotifyBytes {
            len: 0,
            bytes: [0; 8]
        }
    );
    assert_eq!(empty.decoded(rule), Decoded::Malformed);

    let eight = NotifyBytes::from_slice(&[0x01, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(eight.len, 8);
    assert_eq!(eight.decoded(rule), Decoded::Malformed);
}
