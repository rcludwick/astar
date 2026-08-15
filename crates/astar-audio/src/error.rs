// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Error types for the audio I/O layer.

use thiserror::Error;

/// Errors produced by audio device enumeration, stream construction, and
/// stream control on an [`AudioBackend`](crate::AudioBackend) implementation.
#[derive(Debug, Error)]
pub enum AudioError {
    /// The host backend (e.g. `CoreAudio`, WASAPI, ALSA) refused to enumerate
    /// or returned an error from device discovery.
    #[error("device enumeration failed: {0}")]
    Enumeration(String),

    /// The named or indexed device could not be located.
    #[error("device not found: {0}")]
    DeviceNotFound(String),

    /// The device does not support the requested `StreamConfig` and no
    /// fallback resampling path is available.
    #[error("device {device} does not support config (rate={rate}, channels={channels})")]
    UnsupportedConfig {
        /// Friendly device name.
        device: String,
        /// Sample rate that was requested.
        rate: u32,
        /// Channel count that was requested.
        channels: u16,
    },

    /// cpal failed to construct or start the underlying stream.
    #[error("stream build failed: {0}")]
    BuildStream(String),

    /// cpal returned an error while starting (`play`) or stopping (`pause`)
    /// the stream.
    #[error("stream control failed: {0}")]
    StreamControl(String),

    /// The resampler could not be constructed for the given conversion
    /// ratio. Typically indicates a degenerate ratio (zero rate).
    #[error("resampler init failed: {0}")]
    Resampler(String),
}
