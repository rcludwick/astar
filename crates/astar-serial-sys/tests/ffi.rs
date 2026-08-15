// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
use std::ffi::{CStr, CString};

use astar_serial_sys::{IAX_ERR_NULL, iax_serial_autodetect, iax_serial_ptt_tick};
use astar_serial_sys::{IAX_ERR_OPEN, IAX_OK, iax_serial_error_text};
use astar_serial_sys::{IaxKeyLine, IaxRadioLine, IaxRxKeyMode, IaxSerialConfig};
use astar_serial_sys::{IaxSerialTransport, iax_serial_close, iax_serial_open};

#[test]
fn error_text_is_static_and_human_readable() {
    let ok = unsafe { CStr::from_ptr(iax_serial_error_text(IAX_OK)) };
    assert_eq!(ok.to_str().unwrap(), "ok");
    let open = unsafe { CStr::from_ptr(iax_serial_error_text(IAX_ERR_OPEN)) };
    assert_eq!(open.to_str().unwrap(), "serial open failed");
    let unknown = unsafe { CStr::from_ptr(iax_serial_error_text(-999)) };
    assert_eq!(unknown.to_str().unwrap(), "unknown error");
}

#[test]
fn config_maps_to_bridge_config_fields() {
    let cfg = IaxSerialConfig {
        port_path: std::ptr::null(),
        transport: IaxSerialTransport::Tty,
        key_line: IaxKeyLine::Dcd,
        key_active_high: false,
        radio_line: IaxRadioLine::Dtr,
        radio_active_high: true,
        cts_debounce_ms: 50,
        rx_mode: IaxRxKeyMode::RxActivity,
        rx_floor_db: -40.0,
        rx_hang_ms: 300,
    };
    // Exposed for testing via the crate's pub(crate) re-export helpers:
    let bc = astar_serial_sys::test_bridge_config(&cfg);
    assert!(!bc.cts_keyed_high);
    assert!(bc.rts_key_high);
    assert_eq!(bc.cts_debounce.as_millis(), 50);
    assert!((bc.rx_floor_db - (-40.0)).abs() < f32::EPSILON);
    assert_eq!(bc.rx_hang.as_millis(), 300);
}

fn bogus_config(path: &CString) -> IaxSerialConfig {
    IaxSerialConfig {
        port_path: path.as_ptr(),
        transport: IaxSerialTransport::Tty,
        key_line: IaxKeyLine::Cts,
        key_active_high: true,
        radio_line: IaxRadioLine::Rts,
        radio_active_high: true,
        cts_debounce_ms: 30,
        rx_mode: IaxRxKeyMode::RemotePtt,
        rx_floor_db: -45.0,
        rx_hang_ms: 250,
    }
}

#[test]
fn usb_transport_selects_usb_backend_and_ignores_path() {
    // transport=Usb routes to the raw-USB backend and never consults port_path.
    let path = CString::new("/dev/should-be-ignored").unwrap();
    let cfg = IaxSerialConfig {
        transport: IaxSerialTransport::Usb,
        ..bogus_config(&path)
    };
    let plan = unsafe { astar_serial_sys::test_backend_plan(&cfg) };
    assert_eq!(plan, Some((true, None)), "USB ignores port_path");
}

#[test]
fn tty_transport_with_path_uses_the_path() {
    let path = CString::new("/dev/cu.usbserial").unwrap();
    let cfg = IaxSerialConfig {
        transport: IaxSerialTransport::Tty,
        ..bogus_config(&path)
    };
    let plan = unsafe { astar_serial_sys::test_backend_plan(&cfg) };
    assert_eq!(plan, Some((false, Some("/dev/cu.usbserial".to_string()))));
}

#[test]
fn tty_transport_null_path_means_autodetect() {
    let path = CString::new("ignored").unwrap();
    let cfg = IaxSerialConfig {
        transport: IaxSerialTransport::Tty,
        port_path: std::ptr::null(),
        ..bogus_config(&path)
    };
    let plan = unsafe { astar_serial_sys::test_backend_plan(&cfg) };
    assert_eq!(plan, Some((false, None)), "null path = autodetect");
}

#[test]
fn open_with_bogus_path_returns_null() {
    let path = CString::new("/dev/iax-nonexistent-serial").unwrap();
    let cfg = bogus_config(&path);
    let h = unsafe { iax_serial_open(&raw const cfg) };
    assert!(h.is_null());
}

#[test]
fn open_with_null_config_returns_null_and_close_null_is_noop() {
    assert!(unsafe { iax_serial_open(std::ptr::null()) }.is_null());
    unsafe { iax_serial_close(std::ptr::null_mut()) }; // must not crash
}

#[test]
fn error_text_is_secret_free() {
    for code in [IAX_OK, IAX_ERR_OPEN, -1, -2, -4, -5, -6, -999] {
        let t = unsafe { CStr::from_ptr(iax_serial_error_text(code)) }
            .to_str()
            .unwrap()
            .to_lowercase();
        for bad in ["secret", "password", "pass", "token"] {
            assert!(!t.contains(bad), "leaked '{bad}' in: {t}");
        }
    }
}

#[test]
fn tick_null_guards() {
    let mut on = false;
    let mut changed = false;
    let rc = unsafe {
        iax_serial_ptt_tick(
            std::ptr::null_mut(),
            false,
            -60.0,
            &raw mut on,
            &raw mut changed,
        )
    };
    assert_eq!(rc, IAX_ERR_NULL);
}

#[test]
fn autodetect_into_small_buffer_or_no_device_is_negative() {
    // Either no WCH device is attached (IAX_ERR_NO_DEVICE) or one is, but a
    // 1-byte buffer cannot hold its path (IAX_ERR_BUFFER). Both are negative.
    let mut buf = [0i8; 1];
    let rc = unsafe { iax_serial_autodetect(buf.as_mut_ptr(), buf.len()) };
    assert!(rc < 0, "expected negative, got {rc}");
}
