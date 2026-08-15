// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Audio-device helpers shared by the `dial` and `parrot` subcommands. These
//! mirror the resolution logic in `astar-iax`'s own `parrot` example: a name
//! substring resolves (case-insensitively, uniquely) to a concrete device id,
//! and an absent substring falls back to the backend default.

use astar_audio::{AudioBackend, CpalBackend, DeviceInfo, Direction};
use astar_iax::Manager;

/// Resolve an optional device-name substring to a concrete device id. `None`
/// falls back to the backend default for `dir`. A present substring is matched
/// case-insensitively and must hit exactly one usable device (`Duplex` counts
/// either way) — zero or several is a configuration error.
pub fn resolve_device(
    mgr: &Manager,
    query: Option<&str>,
    dir: Direction,
) -> Result<String, String> {
    let Some(q) = query else {
        let dev = match dir {
            Direction::Output => mgr.default_output(),
            _ => mgr.default_input(),
        }
        .ok_or_else(|| format!("no default {dir:?} audio device"))?;
        return Ok(dev.id.as_str().to_string());
    };

    let devices = mgr
        .devices()
        .map_err(|e| format!("audio enumeration failed: {e}"))?;
    let usable = |d: &&DeviceInfo| d.direction == dir || d.direction == Direction::Duplex;
    let needle = q.to_lowercase();
    let matches: Vec<&DeviceInfo> = devices
        .iter()
        .filter(usable)
        .filter(|d| d.name.to_lowercase().contains(&needle))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id.as_str().to_string()),
        [] => Err(format!("no {dir:?} device name contains {q:?}")),
        many => {
            let names: Vec<&str> = many.iter().map(|d| d.name.as_str()).collect();
            Err(format!("{q:?} is ambiguous; matches {names:?}"))
        }
    }
}

/// `--list-devices`: enumerate and print every audio device, then return.
pub fn list_devices() -> Result<(), String> {
    let backend = CpalBackend::new();
    let devices = backend
        .devices()
        .map_err(|e| format!("audio enumeration failed: {e}"))?;
    for d in devices {
        let rates = d
            .native_sample_rates
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join("/");
        println!(
            "{:<40} [{:?}] ch={} rates={}",
            d.name,
            d.direction,
            d.channels,
            if rates.is_empty() {
                "?".to_string()
            } else {
                rates
            },
        );
    }
    Ok(())
}
