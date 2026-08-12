// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Integration smoke test for device enumeration. cpal opens real audio
//! devices, which is allowed to return an empty list in a sandboxed CI
//! environment; we only require that the call doesn't error out.

use astar_audio::CpalBackend;
// `AudioBackend` (the `devices()`/`default_*` trait methods) is only exercised
// by the macOS-gated test below; importing it unconditionally is an unused
// import on Linux under `-D warnings`.
#[cfg(target_os = "macos")]
use astar_audio::AudioBackend;

#[cfg(target_os = "macos")]
#[test]
fn enumerate_devices_does_not_error_on_macos() {
    let backend = CpalBackend::new();
    let devices = backend.devices().expect("devices() returned Err");
    // List may legitimately be empty in a sandboxed run; just check that
    // the call returned Ok and produced valid device entries.
    for d in &devices {
        assert!(!d.name.is_empty(), "device with empty name");
        assert!(!d.id.as_str().is_empty(), "device with empty id");
    }
    // Defaults: may or may not exist; just verify the calls don't panic.
    let _ = backend.default_input();
    let _ = backend.default_output();
}

#[test]
fn cpal_backend_constructs() {
    // Cheap sanity check that survives any host.
    let _ = CpalBackend::new();
}
