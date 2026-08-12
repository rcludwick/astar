// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Audio glue between the injected `AudioBackend` and the call runtime (iax-612e).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Shared push-to-talk flag read by the mic sink each callback.
#[derive(Clone)]
pub(crate) struct PttGate(Arc<AtomicBool>);

impl PttGate {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
    pub(crate) fn set(&self, engaged: bool) {
        self.0.store(engaged, Ordering::Relaxed);
    }
    pub(crate) fn engaged(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use astar_audio::{
        AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
        StreamConfig, StreamHandle,
    };
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Shared {
        recorded_mic: Vec<f32>,
        pending_mic: Vec<f32>,
        sink: Option<Box<dyn InputSink>>,
    }

    #[derive(Clone)]
    pub(crate) struct NullControls(Arc<Mutex<Shared>>);
    pub(crate) struct NullBackend(Arc<Mutex<Shared>>);

    impl NullBackend {
        pub(crate) fn new() -> (Self, NullControls) {
            let s = Arc::new(Mutex::new(Shared::default()));
            (Self(Arc::clone(&s)), NullControls(s))
        }
    }

    impl NullControls {
        pub(crate) fn take_input_sink(&self) -> Box<dyn InputSink> {
            Box::new(RecordingSink(Arc::clone(&self.0)))
        }
        pub(crate) fn push_mic(&self, samples: &[f32]) {
            let mut g = self.0.lock().unwrap();
            if let Some(mut sink) = g.sink.take() {
                drop(g);
                sink.write(samples, 0.0);
                self.0.lock().unwrap().sink = Some(sink);
            } else {
                g.pending_mic.extend_from_slice(samples);
            }
        }
        pub(crate) fn recorded_mic(&self) -> Vec<f32> {
            self.0.lock().unwrap().recorded_mic.clone()
        }
    }

    struct RecordingSink(Arc<Mutex<Shared>>);
    impl InputSink for RecordingSink {
        fn write(&mut self, samples: &[f32], _meter: f32) {
            self.0
                .lock()
                .unwrap()
                .recorded_mic
                .extend_from_slice(samples);
        }
    }

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

    fn dev(dir: Direction, tag: &str) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new(tag.to_string()),
            name: tag.to_string(),
            direction: dir,
            channels: 1,
            native_sample_rates: vec![8_000],
        }
    }

    impl AudioBackend for NullBackend {
        fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            Ok(vec![
                dev(Direction::Input, "null-in"),
                dev(Direction::Output, "null-out"),
            ])
        }
        fn default_input(&self) -> Option<DeviceInfo> {
            Some(dev(Direction::Input, "null-in"))
        }
        fn default_output(&self) -> Option<DeviceInfo> {
            Some(dev(Direction::Output, "null-out"))
        }
        fn open_input(
            &self,
            _d: &DeviceInfo,
            _c: StreamConfig,
            sink: Box<dyn InputSink>,
            _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
        ) -> Result<Box<dyn StreamHandle>, AudioError> {
            self.0.lock().unwrap().sink = Some(sink);
            Ok(Box::new(NullHandle))
        }
        fn open_output(
            &self,
            _d: &DeviceInfo,
            _c: StreamConfig,
            _s: Box<dyn OutputSource>,
        ) -> Result<Box<dyn StreamHandle>, AudioError> {
            Ok(Box::new(NullHandle))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::NullBackend;
    use astar_audio::{AudioBackend, StreamConfig};

    #[test]
    fn null_backend_round_trips_through_sink_and_source() {
        let (backend, ctrls) = NullBackend::new();
        let dev = backend.default_input().expect("null input dev");
        let sink = ctrls.take_input_sink();
        let _h = backend
            .open_input(
                &dev,
                StreamConfig::default(),
                sink,
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            )
            .expect("open input");
        ctrls.push_mic(&[0.5_f32; 160]);
        assert_eq!(ctrls.recorded_mic().len(), 160);
    }
}
