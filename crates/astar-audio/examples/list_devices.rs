// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Print the engine's own view of the host's audio devices.
//!
//! `cargo run -p astar-audio --example list_devices`
//!
//! Prints the `DeviceId` as well as the name, which is the point: `DeviceId`
//! is `"in:<name>"` / `"out:<name>"`, so two devices reporting the same
//! `CoreAudio` name emit the SAME id and only the first is reachable through
//! `find_device`. That is astar-9d41; `apps/macos/Tools/dup-audio-devices.swift`
//! manufactures the condition on a Mac without the hardware.

fn main() {
    let backend = astar_audio::CpalBackend::new();
    let devices = match astar_audio::AudioBackend::devices(&backend) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("enumeration failed: {e}");
            std::process::exit(1);
        }
    };
    for d in &devices {
        println!("{:?}  id={}  name={}", d.direction, d.id.as_str(), d.name);
    }
}
