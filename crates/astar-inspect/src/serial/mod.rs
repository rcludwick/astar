// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! UCI150 serial PTT wiring for the harness (iax-8e3b, rewired via
//! astar-ptt in iax-53da): handset CTS keys the harness, RTS keys the
//! radio while receiving. Env-driven config; the bridge logic and the
//! hardware backend live in the `astar-ptt` crate.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;

use astar_ptt::{
    BridgeConfig, PttBackend, PttIo, RadioKeyInput, RxKeyMode, Uci150Serial, Uci150Usb,
};

use crate::server::ServerState;

/// Which transport drives the UCI150's PTT lines. Selectable at runtime so we
/// can flip between the `CH34x` tty/dext path and the raw-USB `IOKit` path without
/// rebuilding (iax-d937).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PttTransport {
    /// OS tty (`/dev/cu.*`) via `serialport`; needs the WCH `CH34x` dext.
    Tty,
    /// Raw USB via IOKit/nusb; no dext (sandbox/MAS-eligible).
    Usb,
}

/// Env-derived serial-PTT configuration. Same env names as before the
/// astar-ptt rewire.
pub struct SerialConfig {
    /// Explicit port path (`HARNESS_PTT_SERIAL`); `None` → autodetect. Only
    /// used by the tty transport.
    pub port: Option<String>,
    /// Which transport opens the device (`HARNESS_PTT_TRANSPORT`).
    pub transport: PttTransport,
    /// Bridge behaviour (polarity, debounce, radio-key mode).
    pub bridge: BridgeConfig,
}

impl SerialConfig {
    /// Read configuration from an env-like getter (testable).
    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Self {
        let truthy = |k: &str| {
            get(k).is_some_and(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "1" || v == "true" || v == "yes" || v == "on"
            })
        };
        let rx_mode = match get("HARNESS_RX_KEY_MODE").as_deref().map(str::trim) {
            Some("rx-activity") => RxKeyMode::RxActivity,
            _ => RxKeyMode::RemotePtt,
        };
        let rx_floor_db = get("HARNESS_RX_FLOOR_DB")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(-45.0);
        let rx_hang_ms = get("HARNESS_RX_HANG_MS")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(250);
        let transport = match get("HARNESS_PTT_TRANSPORT").as_deref().map(str::trim) {
            Some("usb" | "iokit") => PttTransport::Usb,
            _ => PttTransport::Tty,
        };
        Self {
            port: get("HARNESS_PTT_SERIAL")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            transport,
            bridge: BridgeConfig {
                cts_keyed_high: !truthy("HARNESS_PTT_INVERT"),
                rts_key_high: !truthy("HARNESS_RTS_INVERT"),
                cts_debounce: Duration::from_millis(30),
                rx_mode,
                rx_floor_db,
                rx_hang: Duration::from_millis(rx_hang_ms),
            },
        }
    }
}

/// Open the PTT backend for the configured transport, or `None` (logged) when
/// no device is present / it cannot be opened.
fn open_backend(cfg: &SerialConfig) -> Option<Box<dyn PttBackend>> {
    match cfg.transport {
        PttTransport::Tty => {
            let path = cfg.port.clone().or_else(Uci150Serial::autodetect)?;
            match Uci150Serial::open(&path) {
                Ok(b) => {
                    tracing::info!(target: "astar_inspect::serial", "serial PTT bridge on {path} (tty)");
                    Some(Box::new(b))
                }
                Err(e) => {
                    tracing::warn!(target: "astar_inspect::serial", "cannot open {path}: {e}; bridge disabled");
                    None
                }
            }
        }
        PttTransport::Usb => match Uci150Usb::open() {
            Ok(b) => {
                tracing::info!(target: "astar_inspect::serial", "PTT bridge on raw USB (IOKit, DCD-in/RTS-out)");
                Some(Box::new(b))
            }
            Err(e) => {
                tracing::warn!(target: "astar_inspect::serial", "cannot open raw-USB PTT: {e}; bridge disabled");
                None
            }
        },
    }
}

/// Start the serial PTT bridge if a device is available; `None` (logged) when
/// there is no device or it cannot be opened — the harness runs fine without
/// handset keying. The runner stops when `state.ptt_stop` is set.
pub fn spawn(state: &Arc<ServerState>, cfg: SerialConfig) -> Option<JoinHandle<()>> {
    let backend = open_backend(&cfg)?;

    // Harness-specific glue: hardware PTT keys BOTH the local parrot
    // (parrot_shared.key) AND the network call. Only the network-call leg routes
    // through the Station (set_ptt / snapshot over the shared session); the
    // parrot side-effect stays here because the generic Station::set_ptt path —
    // which a serial PTT source drives — does not touch the parrot.
    let key_state = Arc::clone(state);
    let snap_state = Arc::clone(state);
    let io = PttIo {
        on_key: Box::new(move |on| {
            key_state.parrot_shared.key.store(on, Ordering::Relaxed);
            // NotConnected while idle is fine — hardware can key before a call.
            let _ = key_state.station.set_ptt(on);
        }),
        radio_input: Box::new(move || {
            let snap = snap_state.station.snapshot();
            RadioKeyInput {
                remote_keyed: snap.remote_ptt,
                rx_level_db: snap.rx_level_db,
            }
        }),
    };
    astar_ptt::spawn(backend, cfg.bridge, io, Arc::clone(&state.ptt_stop))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn parse_config_defaults_and_overrides() {
        let empty = SerialConfig::parse(|_| None);
        assert!(empty.port.is_none());
        assert!(empty.bridge.cts_keyed_high);
        assert!(empty.bridge.rts_key_high);
        assert!(matches!(empty.bridge.rx_mode, RxKeyMode::RemotePtt));
        // Default transport stays the tty/dext path — unchanged behaviour.
        assert!(matches!(empty.transport, PttTransport::Tty));

        let mut m = HashMap::new();
        m.insert("HARNESS_PTT_SERIAL", "/dev/cu.x");
        m.insert("HARNESS_PTT_INVERT", "1");
        m.insert("HARNESS_RTS_INVERT", "1");
        m.insert("HARNESS_RX_KEY_MODE", "rx-activity");
        m.insert("HARNESS_RX_FLOOR_DB", "-30");
        m.insert("HARNESS_RX_HANG_MS", "500");
        let cfg = SerialConfig::parse(|k| m.get(k).map(ToString::to_string));
        assert_eq!(cfg.port.as_deref(), Some("/dev/cu.x"));
        assert!(!cfg.bridge.cts_keyed_high);
        assert!(!cfg.bridge.rts_key_high);
        assert!(matches!(cfg.bridge.rx_mode, RxKeyMode::RxActivity));
        assert!((cfg.bridge.rx_floor_db - -30.0).abs() < f32::EPSILON);
        assert_eq!(cfg.bridge.rx_hang, Duration::from_millis(500));
    }

    #[test]
    fn transport_selects_raw_usb_via_env() {
        let usb =
            SerialConfig::parse(|k| (k == "HARNESS_PTT_TRANSPORT").then(|| "usb".to_string()));
        assert!(matches!(usb.transport, PttTransport::Usb));

        // "iokit" is an accepted alias for the raw-USB backend.
        let iokit =
            SerialConfig::parse(|k| (k == "HARNESS_PTT_TRANSPORT").then(|| "iokit".to_string()));
        assert!(matches!(iokit.transport, PttTransport::Usb));

        // Anything else (incl. "tty") stays on the tty/dext path.
        let tty =
            SerialConfig::parse(|k| (k == "HARNESS_PTT_TRANSPORT").then(|| "tty".to_string()));
        assert!(matches!(tty.transport, PttTransport::Tty));
    }
}
