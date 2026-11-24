// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The speaker monitor's playback side: a listen-only copy of an output
//! bus's audio, played on a second device.
//!
//! The bus offers a copy of every buffer it feeds its device into the
//! [`HeardConsumer::Monitor`](crate::HeardConsumer::Monitor) tap. A
//! [`MonitorOutput`] is the [`OutputSource`] of the monitor device's stream:
//! on each of that device's callbacks it drains the tap, resamples the
//! chunks to its own rate, and plays them out of a small ring buffer.
//!
//! The two devices run on separate clocks, so one always runs a little fast
//! against the other. The ring absorbs that: it aims to hold a target fill,
//! and when its (smoothed) fill strays more than [`MONITOR_BAND_MS`] from
//! that it drops or repeats one sample per callback.
//!
//! The target has to cover the largest single read the device makes, or a
//! read can empty the ring before the drift correction ever acts. On a
//! resampled device the backend's `fill_output` reads in gulps of a whole
//! callback plus 512 samples of headroom (592 samples, 74 ms, at 8 kHz), so
//! the target is the largest read seen plus [`MONITOR_READ_MARGIN_MS`],
//! never under [`MONITOR_MIN_TARGET_MS`] or over [`MONITOR_MAX_TARGET_MS`].
//! That costs up to 150 ms of latency, on the monitor only. A few hundred parts per million of drift needs a correction a
//! few times a second, which nobody can hear. When the ring runs dry it
//! plays silence and waits to fill back up to the target before playing
//! again, so a gap is one clean gap rather than a stutter.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Receiver;

use astar_record::tap::PcmChunk;

use crate::resample::AntiAliasResampler;
use crate::stream::OutputSource;

/// The least audio the ring aims to hold, in milliseconds.
pub const MONITOR_MIN_TARGET_MS: u32 = 60;
/// The most audio the ring aims to hold, in milliseconds.
pub const MONITOR_MAX_TARGET_MS: u32 = 150;
/// How much more than the largest read seen the ring aims to hold, in
/// milliseconds.
pub const MONITOR_READ_MARGIN_MS: u32 = 20;
/// How far the ring's fill may stray from the target before it drops or
/// repeats a sample, in milliseconds.
pub const MONITOR_BAND_MS: u32 = 10;
/// The most the ring holds before it throws the oldest audio away, in
/// milliseconds: after the monitor device stalls, catching up beats playing
/// everything late.
pub const MONITOR_MAX_FILL_MS: u32 = 300;
/// Chunks the monitor tap holds between two monitor callbacks: about 200 ms
/// at the 10 ms buffers an output device typically asks for. The monitor
/// drains it on every callback, so it only has to cover one callback's gap.
pub const MONITOR_TAP_CAPACITY: usize = 20;

/// How quickly the smoothed fill follows the raw fill, per callback. The
/// raw fill saws up and down by a whole chunk as audio arrives in bursts;
/// deciding on it directly would drop and repeat samples on every burst.
const FILL_SMOOTHING: f32 = 1.0 / 32.0;

/// Size of the scratch buffer resampled audio is drained through.
const RESAMPLE_SCRATCH: usize = 4_096;

/// The monitor device's [`OutputSource`]: plays what arrives on the
/// monitor tap's receiver, at `rate`, through a drift-correcting ring.
///
/// It writes mono, as [`OutputBus`](crate::router::OutputBus) does; the
/// backend spreads it across the device's channels.
pub struct MonitorOutput {
    /// The rate this source plays at (the monitor stream's config rate).
    pub(crate) rate: u32,
    /// The monitor tap's receiving end. Drained with `try_recv` only: a
    /// receiver parked in `recv` would make the bus's audio thread pay for a
    /// wake-up on every offer.
    rx: Receiver<PcmChunk>,
    /// Monitor gain (f32 bits), shared with the router.
    pub(crate) gain: Arc<AtomicU32>,
    /// Monitor mute, shared with the router.
    pub(crate) muted: Arc<AtomicBool>,
    ring: VecDeque<f32>,
    /// The resampler for the rate chunks last arrived at, when that differs
    /// from `rate`.
    resampler: Option<(u32, AntiAliasResampler)>,
    /// Scratch for one chunk converted to f32.
    conv: Vec<f32>,
    /// Scratch the resampler drains through.
    resampled: Vec<f32>,
    /// Waiting for the ring to fill to the target before playing.
    priming: bool,
    /// The ring's fill after each callback, smoothed (in samples).
    smoothed_fill: f32,
    target: usize,
    band: usize,
    max_fill: usize,
    /// The largest read the device has made, which sizes `target`.
    largest_read: usize,
    min_target: usize,
    max_target: usize,
    margin: usize,
    /// Times the ring ran dry while playing.
    underruns: u64,
}

/// Samples in `ms` milliseconds at `rate`.
fn samples_in(rate: u32, ms: u32) -> usize {
    (u64::from(rate) * u64::from(ms) / 1_000) as usize
}

impl MonitorOutput {
    /// A monitor source playing at `rate` the chunks that arrive on `rx`,
    /// scaled by `gain` (f32 bits) and silenced while `muted` is set.
    #[must_use]
    pub fn new(
        rate: u32,
        rx: Receiver<PcmChunk>,
        gain: Arc<AtomicU32>,
        muted: Arc<AtomicBool>,
    ) -> Self {
        let max_fill = samples_in(rate, MONITOR_MAX_FILL_MS);
        Self {
            rate,
            rx,
            gain,
            muted,
            // Room for the cap plus a full tap of large chunks arriving at
            // once, so pushing never reallocates on the audio thread.
            ring: VecDeque::with_capacity(max_fill * 3),
            resampler: None,
            conv: Vec::with_capacity(RESAMPLE_SCRATCH),
            resampled: vec![0.0; RESAMPLE_SCRATCH],
            priming: true,
            smoothed_fill: 0.0,
            target: samples_in(rate, MONITOR_MIN_TARGET_MS),
            band: samples_in(rate, MONITOR_BAND_MS),
            max_fill,
            largest_read: 0,
            min_target: samples_in(rate, MONITOR_MIN_TARGET_MS),
            max_target: samples_in(rate, MONITOR_MAX_TARGET_MS),
            margin: samples_in(rate, MONITOR_READ_MARGIN_MS),
            underruns: 0,
        }
    }

    /// How many samples the ring holds right now.
    #[must_use]
    pub fn fill(&self) -> usize {
        self.ring.len()
    }

    /// The fill the ring aims for, in samples.
    #[must_use]
    pub fn target(&self) -> usize {
        self.target
    }

    /// How many times the ring has run dry while playing.
    #[must_use]
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// Grow the target to cover a read of `len` samples.
    fn note_read(&mut self, len: usize) {
        if len > self.largest_read {
            self.largest_read = len;
            self.target = (len + self.margin).clamp(self.min_target, self.max_target);
        }
    }

    /// Move everything waiting on the tap into the ring, resampled to
    /// `rate`. Never blocks.
    fn drain_tap(&mut self) {
        while let Ok(chunk) = self.rx.try_recv() {
            self.conv.clear();
            self.conv
                .extend(chunk.samples.iter().map(|&s| f32::from(s) / 32_767.0));
            if chunk.rate == self.rate || chunk.rate == 0 {
                self.ring.extend(self.conv.iter().copied());
            } else {
                if self
                    .resampler
                    .as_ref()
                    .is_none_or(|(r, _)| *r != chunk.rate)
                {
                    // Only on a change of bus rate, which is rare.
                    self.resampler = AntiAliasResampler::new(chunk.rate, self.rate)
                        .ok()
                        .map(|r| (chunk.rate, r));
                }
                let Some((_, rs)) = self.resampler.as_mut() else {
                    continue;
                };
                if rs.push(&self.conv).is_err() {
                    continue;
                }
                loop {
                    let n = rs.drain(&mut self.resampled);
                    if n == 0 {
                        break;
                    }
                    self.ring.extend(self.resampled[..n].iter().copied());
                }
            }
            if self.ring.len() > self.max_fill {
                let excess = self.ring.len() - self.target;
                self.ring.drain(..excess);
            }
        }
    }

    /// Drop or repeat one sample when the smoothed fill has left the band.
    fn correct_drift(&mut self) {
        #[allow(clippy::cast_precision_loss)]
        let fill = self.ring.len() as f32;
        self.smoothed_fill += (fill - self.smoothed_fill) * FILL_SMOOTHING;
        #[allow(clippy::cast_precision_loss)]
        let (hi, lo) = (
            (self.target + self.band) as f32,
            self.target.saturating_sub(self.band) as f32,
        );
        if self.smoothed_fill > hi {
            self.ring.pop_front();
        } else if self.smoothed_fill < lo
            && let Some(&first) = self.ring.front()
        {
            self.ring.push_front(first);
        }
    }
}

impl OutputSource for MonitorOutput {
    fn read(&mut self, out: &mut [f32]) -> usize {
        self.note_read(out.len());
        self.drain_tap();
        if self.priming {
            // Start only once playing this callback still leaves the target
            // behind, so the ring begins at its target, not under it.
            if self.ring.len() < self.target + out.len() {
                out.fill(0.0);
                return out.len();
            }
            self.priming = false;
            #[allow(clippy::cast_precision_loss)]
            {
                self.smoothed_fill = (self.ring.len() - out.len()) as f32;
            }
        }
        let n = self.ring.len().min(out.len());
        for (o, s) in out.iter_mut().zip(self.ring.drain(..n)) {
            *o = s;
        }
        out[n..].fill(0.0);
        if n < out.len() {
            // Ran dry: silence until the ring is back at its target.
            self.priming = true;
            self.underruns += 1;
        } else {
            self.correct_drift();
        }
        if self.muted.load(Ordering::Relaxed) {
            out.fill(0.0);
        } else {
            // Clamped even at unity: the resampler can overshoot full scale.
            let g = f32::from_bits(self.gain.load(Ordering::Relaxed));
            for s in out.iter_mut() {
                *s = (*s * g).clamp(-1.0, 1.0);
            }
        }
        out.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_record::tap::{PcmChunk, tap_channel};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    fn monitor(rate: u32) -> (MonitorOutput, astar_record::tap::Tap<PcmChunk>) {
        let (tap, rx) = tap_channel(MONITOR_TAP_CAPACITY);
        let out = MonitorOutput::new(
            rate,
            rx,
            Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            Arc::new(AtomicBool::new(false)),
        );
        (out, tap)
    }

    /// Runs `seconds` of a feed at `ratio` times the read rate, in 20 ms
    /// chunks against 10 ms reads, and returns the ring's fill (in samples)
    /// after every read once a one-second start-up has passed.
    fn drift_run(ratio: f64, seconds: usize, muted: bool) -> (Vec<usize>, MonitorOutput) {
        const RATE: u32 = 8_000;
        const READ: usize = 80; // 10 ms
        let (mut mon, tap) = monitor(RATE);
        mon.muted.store(muted, Ordering::Relaxed);
        let mut fed = 0_usize;
        let mut phase = 0_i16;
        let mut fills = Vec::new();
        let ticks = seconds * 100;
        for tick in 0..ticks {
            if tick % 2 == 0 {
                // How many samples the feed's clock has produced by now.
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    clippy::cast_precision_loss
                )]
                let due = ((tick + 2) as f64 * READ as f64 * ratio) as usize;
                let samples: Vec<i16> = (fed..due)
                    .map(|_| {
                        phase = (phase + 37) % 4000;
                        1_000 + phase
                    })
                    .collect();
                fed = due;
                assert!(tap.offer(PcmChunk {
                    rate: RATE,
                    samples
                }));
            }
            let mut out = [0.0_f32; READ];
            assert_eq!(crate::OutputSource::read(&mut mon, &mut out), READ);
            if muted {
                assert!(out.iter().all(|&s| s == 0.0), "muted plays silence");
            }
            if tick >= 100 {
                fills.push(mon.fill());
            }
        }
        (fills, mon)
    }

    fn assert_fill_bounded(fills: &[usize], mon: &MonitorOutput) {
        let target = mon.target();
        let band = mon.rate as usize * 20 / 1_000;
        let lo = fills.iter().min().copied().unwrap();
        let hi = fills.iter().max().copied().unwrap();
        assert!(
            lo + band >= target && hi <= target + band,
            "fill {lo}..={hi} samples left target {target} ± {band}"
        );
    }

    #[test]
    fn ring_holds_fill_when_feed_is_300ppm_fast() {
        let (fills, mon) = drift_run(1.0003, 600, false);
        assert_fill_bounded(&fills, &mon);
    }

    #[test]
    fn ring_holds_fill_when_feed_is_300ppm_slow() {
        let (fills, mon) = drift_run(0.9997, 600, false);
        assert_fill_bounded(&fills, &mon);
    }

    /// Both clocks driven the way `fill_output` drives a source on a
    /// resampled 48 kHz device: a 480-frame callback every 10 ms, which at
    /// the 8 kHz pipeline rate is 80 samples, served from a residual that is
    /// topped up by asking the source for 80 + 512 samples whenever it runs
    /// short. The bus side delivers its heard audio in the same gulps, on a
    /// clock `ratio` times as fast. Returns the post-read fills after a
    /// one-second start-up and the underruns counted after it.
    fn fill_output_run(ratio: f64, seconds: usize) -> (Vec<usize>, u64, MonitorOutput) {
        const RATE: u32 = 8_000;
        const CALLBACK: usize = 80;
        const REQUEST: usize = CALLBACK + 512;
        let (mut mon, tap) = monitor(RATE);
        let (mut bus_residual, mut mon_residual) = (0_usize, 0_usize);
        let mut bus_next = 0.0_f64;
        let mut phase = 0_i16;
        let mut fills = Vec::new();
        let mut underruns_at_startup = 0;
        let mut out = vec![0.0_f32; REQUEST];
        for tick in 0..seconds * 100 {
            #[allow(clippy::cast_precision_loss)]
            let now = tick as f64 * 0.010;
            while bus_next <= now {
                if bus_residual < CALLBACK {
                    let samples: Vec<i16> = (0..REQUEST)
                        .map(|_| {
                            phase = (phase + 37) % 4000;
                            1_000 + phase
                        })
                        .collect();
                    assert!(tap.offer(PcmChunk {
                        rate: RATE,
                        samples
                    }));
                    bus_residual += REQUEST;
                }
                bus_residual -= CALLBACK;
                bus_next += 0.010 / ratio;
            }
            if mon_residual < CALLBACK {
                assert_eq!(crate::OutputSource::read(&mut mon, &mut out), REQUEST);
                mon_residual += REQUEST;
                if tick >= 100 {
                    fills.push(mon.fill());
                }
            }
            mon_residual -= CALLBACK;
            if tick == 100 {
                underruns_at_startup = mon.underruns();
            }
        }
        let underruns = mon.underruns() - underruns_at_startup;
        (fills, underruns, mon)
    }

    fn assert_rides_fill_output(ratio: f64) {
        let (fills, underruns, mon) = fill_output_run(ratio, 600);
        assert_eq!(underruns, 0, "the ring ran dry after start-up");
        // One read is a whole 592-sample gulp, so the post-read fill swings
        // by up to a gulp as the two clocks slide in phase; it must stay
        // inside that and never reach the cap.
        let target = mon.target();
        let band = mon.rate as usize * 10 / 1_000;
        let hi = fills.iter().max().copied().unwrap();
        assert!(
            target >= 592 + 160,
            "target {target} covers a read plus 20 ms"
        );
        assert!(
            hi <= target + 592 + band,
            "fill reached {hi} against target {target}"
        );
    }

    #[test]
    fn ring_rides_fill_output_reads_300ppm_fast() {
        assert_rides_fill_output(1.0003);
    }

    #[test]
    fn ring_rides_fill_output_reads_300ppm_slow() {
        assert_rides_fill_output(0.9997);
    }

    #[test]
    fn output_is_clamped_at_unity_gain() {
        let (mut mon, tap) = monitor(8_000);
        // Full-scale input that a resampler could overshoot; here a value
        // past full scale stands in for it, through the passthrough path.
        for _ in 0..10 {
            assert!(tap.offer(PcmChunk {
                rate: 8_000,
                samples: vec![i16::MIN; 160],
            }));
        }
        let mut out = [0.0_f32; 160];
        crate::OutputSource::read(&mut mon, &mut out);
        assert!(out.iter().all(|&s| s >= -1.0), "clamped to -1.0");
    }

    #[test]
    fn the_target_grows_to_cover_the_largest_read() {
        let (mut mon, _tap) = monitor(8_000);
        assert_eq!(mon.target(), 480, "60 ms floor");
        let mut out = vec![0.0_f32; 592];
        crate::OutputSource::read(&mut mon, &mut out);
        assert_eq!(mon.target(), 592 + 160, "read plus 20 ms");
        let mut out = vec![0.0_f32; 4_000];
        crate::OutputSource::read(&mut mon, &mut out);
        assert_eq!(mon.target(), 1_200, "150 ms ceiling");
    }

    #[test]
    fn muted_monitor_plays_silence_but_keeps_draining() {
        // A fast feed: if muting stopped the ring draining, it would grow.
        let (fills, mon) = drift_run(1.0003, 60, true);
        assert_fill_bounded(&fills, &mon);
    }

    #[test]
    fn underrun_plays_silence() {
        let (mut mon, _tap) = monitor(8_000);
        let mut out = [0.7_f32; 160];
        assert_eq!(crate::OutputSource::read(&mut mon, &mut out), out.len());
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn gain_scales_and_clamps() {
        let (mut mon, tap) = monitor(8_000);
        mon.gain.store(2.0_f32.to_bits(), Ordering::Relaxed);
        for _ in 0..10 {
            assert!(tap.offer(PcmChunk {
                rate: 8_000,
                samples: vec![20_000; 160],
            }));
        }
        let mut out = [0.0_f32; 160];
        crate::OutputSource::read(&mut mon, &mut out);
        assert!(
            out.iter().all(|&s| (s - 1.0).abs() < f32::EPSILON),
            "clamped to full scale"
        );
    }

    #[test]
    fn monitor_resamples_bus_rate_to_device_rate() {
        const BUS: u32 = 48_000;
        const DEVICE: u32 = 44_100;
        let (mut mon, tap) = monitor(DEVICE);
        let mut t = 0_usize;
        let mut played = Vec::new();
        for _ in 0..300 {
            // 10 ms of a 1 kHz sine at the bus rate.
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            let samples: Vec<i16> = (0..480)
                .map(|_| {
                    t += 1;
                    let x = (std::f64::consts::TAU * 1_000.0 * t as f64 / f64::from(BUS)).sin();
                    (x * 16_000.0) as i16
                })
                .collect();
            assert!(tap.offer(PcmChunk { rate: BUS, samples }));
            let mut out = [0.0_f32; 441];
            crate::OutputSource::read(&mut mon, &mut out);
            played.extend_from_slice(&out);
        }
        // The last two seconds, well past the start-up.
        let tail = &played[played.len() - 2 * DEVICE as usize..];
        let crossings = tail
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        #[allow(clippy::cast_precision_loss)]
        let hz = crossings as f64 / 2.0 / 2.0;
        assert!((hz - 1_000.0).abs() <= 10.0, "played {hz} Hz, wanted 1 kHz");
        // And the ring stayed bounded with the rates balanced.
        let band = DEVICE as usize * 20 / 1_000;
        assert!(mon.fill() <= mon.target() + band, "fill {}", mon.fill());
    }
}
