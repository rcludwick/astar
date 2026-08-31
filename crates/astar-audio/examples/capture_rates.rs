// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! What capture rates does each input device on this machine actually offer?
//!
//! `docs/design/noise-suppression.md` prefers 48 kHz at open because a
//! 48 kHz-trained stage cannot be resampled into. It also records that
//! "most devices whose default is 44.1 kHz still support 48 kHz" was an
//! expectation rather than a measurement. This is the measurement: run it
//! on a machine and read the last line.
//!
//! ```text
//! cargo run -p astar-audio --example capture_rates
//! ```

use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        println!("no input devices (host refused enumeration)");
        return;
    };

    let (mut rescued, mut stuck, mut already) = (0, 0, 0);
    for dev in devices {
        let name = dev
            .description()
            .map_or_else(|_| "<unknown>".to_string(), |d| d.name().to_string());
        let Ok(default) = dev.default_input_config() else {
            println!("{name}: no default input config");
            continue;
        };
        let ranges: Vec<(u32, u32)> = dev
            .supported_input_configs()
            .map(|it| {
                it.filter(|r| {
                    r.channels() == default.channels()
                        && r.sample_format() == default.sample_format()
                })
                .map(|r| (r.min_sample_rate(), r.max_sample_rate()))
                .collect()
            })
            .unwrap_or_default();

        let offers_48k = ranges
            .iter()
            .any(|&(lo, hi)| (lo..=hi).contains(&astar_audio::PREFERRED_CAPTURE_RATE));
        let verdict = match (default.sample_rate() == 48_000, offers_48k) {
            (true, _) => {
                already += 1;
                "already 48k"
            }
            (false, true) => {
                rescued += 1;
                "RESCUED by the preference"
            }
            (false, false) => {
                stuck += 1;
                "cannot do 48k — falls back to the filter+gate chain"
            }
        };
        println!(
            "{name}\n  default {} Hz, {} ch, {:?}\n  ranges {:?}\n  -> {verdict}",
            default.sample_rate(),
            default.channels(),
            default.sample_format(),
            ranges,
        );
    }
    println!("\nalready 48k: {already} · rescued: {rescued} · stuck below 48k: {stuck}");
}
