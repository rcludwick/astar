// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! P1: the router surfaces per-mic/per-bus control + metering cells so a higher
//! layer can drive gain/DSP/levels on an open device after the call audio is
//! unified onto the router lanes.

use astar_audio::stream::test_support_router::NullBackend;
use astar_audio::{AudioRouter, MicId, OutputId, StreamConfig};

fn router() -> AudioRouter {
    AudioRouter::new(Box::new(NullBackend::new()))
}

#[test]
fn mic_control_cells_are_reachable_after_open() {
    let mut r = router();
    let mic = MicId::new("in:test");
    let out = OutputId::new("out:test");
    r.open_call(&mic, &out, StreamConfig::default()).unwrap();
    r.set_mic_gain(&mic, 1.5);
    r.set_mic_denoise(&mic, true);
    r.set_mic_compress(&mic, true);
    assert!((r.mic_gain(&mic).unwrap() - 1.5).abs() < 1e-6);
    assert!(r.mic_tx_dbfs(&mic).unwrap() <= 0.0);
}

#[test]
fn output_gain_and_level_reachable_after_open() {
    let mut r = router();
    let out = OutputId::new("out:test");
    r.add_rx_to_bus(&out, std::sync::mpsc::channel().1, StreamConfig::default())
        .unwrap();
    r.set_output_gain(&out, 0.5);
    assert!((r.output_gain(&out).unwrap() - 0.5).abs() < 1e-6);
    assert!(r.output_rx_dbfs(&out).unwrap() <= 0.0);
}

#[test]
fn accessors_on_unopened_device_return_none() {
    let r = router();
    assert!(r.mic_gain(&MicId::new("nope")).is_none());
    assert!(r.output_gain(&OutputId::new("nope")).is_none());
}
