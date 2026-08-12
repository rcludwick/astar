// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Audio device I/O for astar.
//!
//! Replaces the `PortAudio` binding the vendored C astar-iax used with a
//! Rust-native cpal-backed implementation. The public surface is the
//! [`AudioBackend`] trait and the [`CpalBackend`] default implementation.
//!
//! # Quick start
//!
//! ```no_run
//! use astar_audio::{AudioBackend, CpalBackend, InputSink, StreamConfig};
//!
//! struct PrintMeter;
//! impl InputSink for PrintMeter {
//!     fn write(&mut self, samples: &[f32], meter: f32) {
//!         println!("got {} samples, peak={meter:.3}", samples.len());
//!     }
//! }
//!
//! let backend = CpalBackend::new();
//! let dev = backend.default_input().expect("no default input");
//! // `overruns` is a shared cumulative capture-overrun counter (iax-9e55).
//! let overruns = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
//! let stream = backend
//!     .open_input(&dev, StreamConfig::default(), Box::new(PrintMeter), overruns)
//!     .expect("open");
//! std::thread::sleep(std::time::Duration::from_secs(1));
//! stream.stop();
//! ```
//!
//! # Sample rate
//!
//! IAX2 voice is 8 kHz mono. The default [`StreamConfig`] reflects that.
//! If the device's native rate differs (very common — UCI150 typically
//! reports 48 kHz), the backend opens cpal at the device's preferred rate
//! and resamples inline using the anti-aliased `rubato::SincFixedIn`-backed
//! [`AntiAliasResampler`] (iax-6945). See the [`resample`] module for the
//! rationale.

mod announce;
pub mod characterize;
pub mod conference;
pub mod denoise;
pub mod device;
pub mod dtmf;
pub mod dynamics;
pub mod error;
pub mod filter;
pub mod meter;
pub mod mixer;
pub mod monitor;
#[cfg(any(test, feature = "test-backend"))]
pub mod null_backend;
pub mod resample;
pub mod router;
mod signal_report;
pub mod spectrum;
pub mod stream;

pub use announce::{AnnounceHandle, AnnouncePolicy};
pub use characterize::{CharacterizeOpts, MicProfile, NotchSpec, characterize, characterize_with};
pub use conference::{Conference, ConferenceConfig, MemberId, ParrotTuning};
pub use denoise::NoiseReducer;
pub use device::{DeviceId, DeviceInfo, Direction};
pub use dtmf::DtmfDetector;
pub use dynamics::{Compressor, CompressorParams, NoiseGate, NoiseGateParams};
pub use error::AudioError;
pub use filter::{Biquad, HumFilter};
pub use meter::{peak, peak_to_dbfs};
pub use mixer::{MixCallId, Mixer};
pub use monitor::MicMonitor;
#[cfg(any(test, feature = "test-backend"))]
pub use null_backend::NullBackend;
pub use resample::{AntiAliasResampler, Resampler1, resample_offline};
pub use router::{AudioRouter, CallAudio, MicId, OutputId};
pub use signal_report::{Grade, SignalReport, analyze_signal, render_report, signal_report_text};
pub use spectrum::{SPECTRUM_BINS, SpectrumAnalyzer};
pub use stream::{AudioBackend, CpalBackend, InputSink, OutputSource, StreamConfig, StreamHandle};
