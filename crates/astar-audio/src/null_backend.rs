// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A hardware-free [`AudioBackend`](crate::AudioBackend) for tests and
//! examples. Gated behind the `test-backend` feature so it never ships in a
//! default build.
//!
//! Unlike the in-crate `test_support_router::NullBackend` (which is
//! `pub(crate)` + `#[cfg(test)]` and exists only for the router unit tests),
//! this type is part of the public API under the feature gate, so downstream
//! crates (notably `astar-station`) can construct a session without
//! touching real audio devices. It reports one synthetic input and one
//! synthetic output device and returns no-op stream handles that produce and
//! consume silence.

use crate::device::{DeviceId, DeviceInfo, Direction};
use crate::error::AudioError;
use crate::stream::{AudioBackend, InputSink, OutputSource, StreamConfig, StreamHandle};

/// A backend that touches no hardware: it advertises a single synthetic input
/// (`in:null`) and output (`out:null`) device and opens streams that drop
/// silently. Construct with [`NullBackend::new`].
#[derive(Debug, Default, Clone, Copy)]
pub struct NullBackend;

impl NullBackend {
    /// Create a `NullBackend`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// No-op stream handle: stopping, pausing, and resuming all succeed without
/// touching any device.
struct NullHandle;

impl StreamHandle for NullHandle {
    fn stop(self: Box<Self>) {}
    fn pause(&self) -> Result<(), AudioError> {
        Ok(())
    }
    fn resume(&self) -> Result<(), AudioError> {
        Ok(())
    }
}

fn dev(direction: Direction, tag: &str) -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new(tag.to_string()),
        name: tag.to_string(),
        direction,
        channels: 1,
        native_sample_rates: vec![8_000],
    }
}

impl AudioBackend for NullBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "in:null"),
            dev(Direction::Output, "out:null"),
        ])
    }

    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "in:null"))
    }

    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "out:null"))
    }

    fn open_input(
        &self,
        _device: &DeviceInfo,
        _config: StreamConfig,
        _sink: Box<dyn InputSink>,
        _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        // Drop the sink: no capture thread, so silence is "delivered" and no
        // overruns are ever counted.
        Ok(Box::new(NullHandle))
    }

    fn open_output(
        &self,
        _device: &DeviceInfo,
        _config: StreamConfig,
        _source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        // Drop the source: nothing pulls samples, so it renders nothing.
        Ok(Box::new(NullHandle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioBackend;

    #[test]
    fn null_backend_lists_two_devices() {
        let b = NullBackend::new();
        let devs = b.devices().expect("devices");
        assert!(devs.iter().any(|d| d.direction == crate::Direction::Input));
        assert!(devs.iter().any(|d| d.direction == crate::Direction::Output));
    }
}
