// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Monitor mode (iax-2377): open the input device and run the mic lane WITHOUT
//! a call, routing post-resample 8 kHz samples to a null/monitor sink (no µ-law
//! encode, no TX), so a front-end can preview/characterize the mic before
//! dialing.
//!
//! [`MicMonitor`] owns its own [`AudioRouter`] and opens exactly one mic with a
//! [`MonitorTap`] sink. The tap retains a bounded ring of the most recent
//! samples (fed to `characterize()`, iax-5fb6). The session never double-opens
//! the device: a caller that already has a live call shares that call's mic lane
//! instead of starting a monitor (the no-op-when-active guard lives in the
//! session layer).

use std::sync::{Arc, Mutex};

use crate::characterize::{CharacterizeOpts, MicProfile, characterize_with};
use crate::spectrum::SpectrumAnalyzer;
use crate::{AudioBackend, AudioError, InputSink, MicId, StreamConfig};

/// How many recent 8 kHz samples the monitor retains for `characterize()`.
/// ~3 s at 8 kHz — enough silence for a stable noise profile, bounded so a
/// long monitor session never grows without limit.
pub const CAPTURE_RING: usize = 8_000 * 3;

/// Shared monitor state the tap (audio thread) writes and the control side
/// reads: the most-recent-samples ring plus the live spectrum analyzer.
struct TapState {
    /// Bounded ring of recent post-resample samples for `characterize()`.
    ring: Vec<f32>,
    /// Log-binned, peak-held spectrum of the monitored mic (iax-e73e).
    spectrum: SpectrumAnalyzer,
}

/// The monitor sink: receives post-resample 8 kHz mono samples and feeds them
/// to the spectrum analyzer + the capture ring. No encode, no TX — it is a pure
/// observer.
struct MonitorTap {
    shared: Arc<Mutex<TapState>>,
}

impl InputSink for MonitorTap {
    fn write(&mut self, samples: &[f32], _meter: f32) {
        let mut st = self.shared.lock().expect("monitor tap mutex");
        st.spectrum.push(samples);
        // Append, then trim from the front so the ring holds the most recent
        // CAPTURE_RING samples (cheap: one extend + one drain on overflow).
        st.ring.extend_from_slice(samples);
        let len = st.ring.len();
        if len > CAPTURE_RING {
            st.ring.drain(..len - CAPTURE_RING);
        }
    }
}

/// A standalone mic monitor: its own router + one open input device, tapping
/// post-resample audio for spectrum/characterization without a call.
pub struct MicMonitor {
    router: crate::AudioRouter,
    mic: MicId,
    shared: Arc<Mutex<TapState>>,
    sample_rate: u32,
}

impl MicMonitor {
    /// Open `input` on a fresh router with a monitor tap and start capturing.
    /// `input` is a device-id string (already resolved by the caller). The
    /// device is opened exactly once; dropping the [`MicMonitor`] releases it.
    ///
    /// # Errors
    /// Propagates [`AudioError`] from device resolution / stream open.
    pub fn start(
        backend: Box<dyn AudioBackend>,
        input: &str,
        config: StreamConfig,
    ) -> Result<Self, AudioError> {
        let shared = Arc::new(Mutex::new(TapState {
            ring: Vec::with_capacity(CAPTURE_RING),
            spectrum: SpectrumAnalyzer::new(config.sample_rate),
        }));
        let mut router = crate::AudioRouter::new(backend);
        let mic = MicId::new(input.to_string());
        let tap = MonitorTap {
            shared: Arc::clone(&shared),
        };
        router.open_input_with(&mic, config, Box::new(tap))?;
        Ok(Self {
            router,
            mic,
            shared,
            sample_rate: config.sample_rate,
        })
    }

    /// The monitored mic id.
    #[must_use]
    pub fn mic(&self) -> &MicId {
        &self.mic
    }

    /// Number of mics the monitor router holds open (always 1 while monitoring).
    #[must_use]
    pub fn mic_count(&self) -> usize {
        self.router.mic_count()
    }

    /// Copy the current log-binned, peak-held spectrum (dBFS) into `out` (up to
    /// `out.len()` bins) and return the number of bins written (iax-e73e). The
    /// front-end polls this ~20 Hz to draw the live mic spectrum.
    pub fn spectrum_into(&self, out: &mut [f32]) -> usize {
        self.shared
            .lock()
            .expect("monitor tap mutex")
            .spectrum
            .copy_into(out)
    }

    /// Set the spectrum peak-hold decay (dB/SECOND, clamped, iax-8616) on the
    /// monitor's analyzer. Takes effect immediately on the live monitored
    /// spectrum so a front-end can scrub the fall-rate while previewing the mic.
    pub fn set_spectrum_decay(&self, db_per_sec: f32) {
        self.shared
            .lock()
            .expect("monitor tap mutex")
            .spectrum
            .set_decay_db_per_sec(db_per_sec);
    }

    /// Run `characterize()` over the retained capture ring with the given
    /// options and return the resulting [`MicProfile`] (iax-5fb6). Call after a
    /// few seconds of monitored silence; the profile is secret-free (plain DSP
    /// numbers). With `opts.harmonic_comb` off this is today's flat detector.
    #[must_use]
    pub fn characterize(&self, opts: CharacterizeOpts) -> MicProfile {
        let ring = {
            let st = self.shared.lock().expect("monitor tap mutex");
            st.ring.clone()
        };
        characterize_with(&ring, self.sample_rate, opts)
    }

    /// Number of samples currently retained for characterization (testing/diag).
    #[must_use]
    pub fn captured_len(&self) -> usize {
        self.shared.lock().expect("monitor tap mutex").ring.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::test_support_router::NullBackend;

    #[test]
    fn monitor_start_opens_exactly_one_mic() {
        let backend: Box<dyn AudioBackend> = Box::new(NullBackend::new());
        let mon = MicMonitor::start(backend, "in:test", StreamConfig::default())
            .expect("monitor opens the input device");
        assert_eq!(mon.mic_count(), 1, "monitor opens exactly one mic");
        assert_eq!(mon.mic().as_str(), "in:test");
    }

    #[test]
    fn monitor_releases_the_device_on_drop() {
        // Dropping the monitor drops its router (and thus the stream handle).
        // The NullBackend handle's `stop` is a no-op, but the Drop path is
        // exercised; a live cpal backend releases the device here.
        let backend: Box<dyn AudioBackend> = Box::new(NullBackend::new());
        let mon = MicMonitor::start(backend, "in:test", StreamConfig::default()).unwrap();
        drop(mon); // no panic, router + handle released
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn monitor_spectrum_reflects_a_driven_tone() {
        use crate::spectrum::{SPECTRUM_BINS, SpectrumAnalyzer};
        use std::f32::consts::TAU;
        let (backend, controls) = NullBackend::with_controls();
        let mon = MicMonitor::start(Box::new(backend), "in:test", StreamConfig::default()).unwrap();
        // Drive a 1 kHz tone through the monitor tap.
        let tone: Vec<f32> = (0..(2_048 * 4))
            .map(|i| 0.8 * (TAU * 1_000.0 * i as f32 / 8_000.0).sin())
            .collect();
        controls.push_mic(&tone);
        let mut out = [0.0_f32; SPECTRUM_BINS];
        let n = mon.spectrum_into(&mut out);
        assert_eq!(n, SPECTRUM_BINS);
        let bin = SpectrumAnalyzer::new(8_000).bin_of_hz(1_000.0);
        assert!(
            out[bin] > -30.0,
            "1 kHz monitored tone should light its log bin: {}",
            out[bin]
        );
    }

    #[test]
    fn monitor_retains_captured_samples_bounded() {
        // Drive more than CAPTURE_RING samples through the tap and confirm the
        // ring stays bounded to the most-recent window.
        let (backend, controls) = NullBackend::with_controls();
        let mon = MicMonitor::start(Box::new(backend), "in:test", StreamConfig::default()).unwrap();
        for _ in 0..((CAPTURE_RING / 160) + 50) {
            controls.push_mic(&[0.1_f32; 160]);
        }
        assert_eq!(
            mon.captured_len(),
            CAPTURE_RING,
            "capture ring is bounded to the most-recent window"
        );
    }
}
