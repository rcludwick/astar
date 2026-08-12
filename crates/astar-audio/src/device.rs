// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Audio device discovery and friendly metadata.
//!
//! [`DeviceInfo`] is the backend-agnostic description handed back from
//! [`AudioBackend::devices`](crate::AudioBackend::devices). Identifiers are
//! opaque strings — for the cpal backend that's `cpal::Device::name()` plus a
//! direction tag, which is stable enough for the macOS `CoreAudio` host where
//! astar's UCI150 lives. We deliberately do *not* try to recognise the
//! UCI150 specifically; whatever string `CoreAudio` reports flows through.

/// Direction of a device's stream support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Device exposes input (capture) streams only.
    Input,
    /// Device exposes output (playback) streams only.
    Output,
    /// Device exposes both input and output.
    Duplex,
}

/// Opaque identifier for a device. The string is whatever the backend uses
/// to look the device up — for cpal that's the device name plus direction.
///
/// Treat as opaque; do not parse.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId(pub String);

impl DeviceId {
    /// Wrap a backend-specific identifier string.
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Metadata for a discovered audio device.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Backend-specific identifier; pass back to `open_input` / `open_output`.
    pub id: DeviceId,
    /// Friendly human-readable name. On macOS `CoreAudio` this is whatever
    /// the device reports.
    pub name: String,
    /// Whether the device supports input, output, or both.
    pub direction: Direction,
    /// Native channel count reported by the device's default config.
    pub channels: u16,
    /// Sample rates the device advertises. May be empty if the backend
    /// only exposes a range; in that case the caller should attempt the
    /// requested rate and fall back to resampling if construction fails.
    pub native_sample_rates: Vec<u32>,
}
