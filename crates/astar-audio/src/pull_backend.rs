// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A headless [`AudioBackend`] for receive-only daemons: it has no sound
//! device, only a thread that pulls the output bus on a steady clock and
//! hands each tick's samples to a sink.
//!
//! The clock is a deadline clock. Tick `k` is due at `start + k * buffer_ms`,
//! and the thread sleeps until that instant, so the time a read or the sink
//! takes never accumulates into drift the way a fixed sleep would.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::device::{DeviceId, DeviceInfo, Direction};
use crate::error::AudioError;
use crate::stream::{AudioBackend, InputSink, OutputSource, StreamConfig, StreamHandle};

/// Receives each pulled frame. It runs on the pull thread, so it must not
/// block: hand the frame to a channel or a lock-free queue and return. It
/// must also not stop or drop the stream handle from inside the callback:
/// stopping joins the pull thread, which would then be waiting on itself.
pub type PullSink = Arc<dyn Fn(PulledFrame) + Send + Sync>;

/// What one tick pulled off the output bus.
pub struct PulledFrame {
    /// Sample rate of `samples`, in Hz.
    pub rate: u32,
    /// The real samples the bus produced, as 16-bit PCM. Empty when the bus
    /// had nothing this tick.
    pub samples: Vec<i16>,
}

/// A backend with one output "device" and no input.
pub struct PullBackend {
    sink: PullSink,
}

impl PullBackend {
    /// Build a backend that delivers every pulled frame to `sink`.
    #[must_use]
    pub fn new(sink: PullSink) -> Self {
        Self { sink }
    }
}

fn pull_device() -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new("netrec-pull".to_string()),
        name: "netrec-pull".to_string(),
        direction: Direction::Output,
        channels: 1,
        native_sample_rates: vec![8_000],
    }
}

impl AudioBackend for PullBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![pull_device()])
    }

    fn default_input(&self) -> Option<DeviceInfo> {
        None
    }

    fn default_output(&self) -> Option<DeviceInfo> {
        Some(pull_device())
    }

    fn open_input(
        &self,
        _device: &DeviceInfo,
        _config: StreamConfig,
        _sink: Box<dyn InputSink>,
        _overruns: Arc<AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Err(AudioError::DeviceNotFound(
            "pull backend has no input".into(),
        ))
    }

    fn open_output(
        &self,
        _device: &DeviceInfo,
        config: StreamConfig,
        mut source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        let rate = config.sample_rate;
        let period = Duration::from_millis(u64::from(config.buffer_ms.max(1)));
        let len = ((u64::from(rate) * u64::from(config.buffer_ms.max(1))) / 1_000)
            .max(1)
            .saturating_mul(u64::from(config.channels.max(1))) as usize;
        let sink = Arc::clone(&self.sink);
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let (t_stop, t_paused) = (Arc::clone(&stop), Arc::clone(&paused));

        let join = thread::Builder::new()
            .name("pull-backend".to_string())
            .spawn(move || {
                let mut buf = vec![0.0_f32; len];
                let mut start = Instant::now();
                let mut k: u32 = 0;
                let mut was_paused = false;
                while !t_stop.load(Ordering::Relaxed) {
                    if t_paused.load(Ordering::Relaxed) {
                        was_paused = true;
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    if was_paused {
                        // Resume from now, not from the old schedule, or the
                        // missed ticks would all fire at once.
                        was_paused = false;
                        start = Instant::now();
                        k = 0;
                    }
                    buf.fill(0.0);
                    let n = source.read(&mut buf).min(buf.len());
                    let samples = buf[..n]
                        .iter()
                        .map(|s| {
                            // Clamped to [-1, 1] first, so this never wraps.
                            #[allow(clippy::cast_possible_truncation)]
                            let v = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                            v
                        })
                        .collect();
                    sink(PulledFrame { rate, samples });

                    k = k.saturating_add(1);
                    let due = start + period * k;
                    let now = Instant::now();
                    if due > now {
                        thread::sleep(due - now);
                    } else if now - due > period * 5 {
                        // Far behind (suspended host, stalled sink): restart
                        // the schedule rather than fire a burst of catch-up
                        // ticks.
                        // The next tick is one period away, not due now.
                        start = now;
                        k = 1;
                        thread::sleep(period);
                    }
                }
            })
            .map_err(|e| AudioError::BuildStream(format!("spawn pull thread: {e}")))?;

        Ok(Box::new(PullHandle {
            stop,
            paused,
            join: Some(join),
        }))
    }
}

struct PullHandle {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl PullHandle {
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl StreamHandle for PullHandle {
    fn stop(mut self: Box<Self>) {
        self.shutdown();
    }
    fn pause(&self) -> Result<(), AudioError> {
        self.paused.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn resume(&self) -> Result<(), AudioError> {
        self.paused.store(false, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for PullHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    struct Ramp {
        reads: Arc<AtomicUsize>,
        n: Option<usize>,
        counter: f32,
    }
    impl OutputSource for Ramp {
        fn read(&mut self, out: &mut [f32]) -> usize {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let n = self.n.unwrap_or(out.len()).min(out.len());
            for s in &mut out[..n] {
                self.counter += 0.001;
                *s = self.counter;
            }
            n
        }
    }

    fn cfg() -> StreamConfig {
        StreamConfig {
            sample_rate: 8_000,
            channels: 1,
            buffer_ms: 20,
        }
    }

    type Opened = (
        Box<dyn StreamHandle>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<PulledFrame>>>,
    );

    fn open(n: Option<usize>) -> Opened {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let f2 = Arc::clone(&frames);
        let backend = PullBackend::new(Arc::new(move |f| f2.lock().unwrap().push(f)));
        let reads = Arc::new(AtomicUsize::new(0));
        let dev = backend.default_output().expect("output");
        let h = backend
            .open_output(
                &dev,
                cfg(),
                Box::new(Ramp {
                    reads: Arc::clone(&reads),
                    n,
                    counter: 0.0,
                }),
            )
            .expect("open");
        (h, reads, frames)
    }

    #[test]
    fn no_input_device() {
        let b = PullBackend::new(Arc::new(|_| {}));
        assert!(b.default_input().is_none());
        let dev = b.default_output().unwrap();
        let r = b.open_input(
            &dev,
            cfg(),
            Box::new(NoSink),
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        );
        assert!(r.is_err());
    }

    struct NoSink;
    impl InputSink for NoSink {
        fn write(&mut self, _s: &[f32], _m: f32) {}
    }

    /// A source that takes ~5 ms per read and records when it was first and
    /// last read, so the test measures the clock from inside the pull thread.
    struct Slow {
        reads: Arc<AtomicUsize>,
        span: Arc<Mutex<Option<(Instant, Instant)>>>,
    }
    impl OutputSource for Slow {
        fn read(&mut self, _out: &mut [f32]) -> usize {
            let now = Instant::now();
            self.reads.fetch_add(1, Ordering::SeqCst);
            let mut span = self.span.lock().unwrap();
            let first = span.map_or(now, |(f, _)| f);
            *span = Some((first, now));
            drop(span);
            std::thread::sleep(Duration::from_millis(5));
            0
        }
    }

    #[test]
    fn pulls_on_a_steady_clock() {
        let reads = Arc::new(AtomicUsize::new(0));
        let span = Arc::new(Mutex::new(None));
        let backend = PullBackend::new(Arc::new(|_| {}));
        let dev = backend.default_output().unwrap();
        let h = backend
            .open_output(
                &dev,
                cfg(),
                Box::new(Slow {
                    reads: Arc::clone(&reads),
                    span: Arc::clone(&span),
                }),
            )
            .unwrap();
        std::thread::sleep(Duration::from_secs(1));
        h.stop();
        let n = reads.load(Ordering::SeqCst);
        let (first, last) = span.lock().unwrap().expect("read at least once");
        let elapsed = last.duration_since(first);
        // Reads happen at 0, 20, 40 ... ms, so a steady clock makes
        // elapsed/20ms + 1 of them. A fixed 20 ms sleep after a 5 ms read
        // would manage only elapsed/25ms.
        let ticks = elapsed.as_secs_f64() / 0.020;
        let n_f = f64::from(u32::try_from(n).unwrap());
        assert!(
            n_f <= ticks + 2.0 && n_f >= ticks * 0.8,
            "{n} reads over {elapsed:?} (expected about {ticks:.1})"
        );
    }

    #[test]
    fn passes_only_real_samples() {
        let (h, _r, frames) = open(Some(0));
        std::thread::sleep(Duration::from_millis(100));
        h.stop();
        let f = frames.lock().unwrap();
        assert!(!f.is_empty());
        assert!(f.iter().all(|x| x.samples.is_empty() && x.rate == 8_000));
        drop(f);

        let (h, _r, frames) = open(Some(160));
        std::thread::sleep(Duration::from_millis(100));
        h.stop();
        let f = frames.lock().unwrap();
        assert!(!f.is_empty());
        assert!(f.iter().all(|x| x.samples.len() == 160));
        assert!(f[0].samples[0] > 0);
    }

    #[test]
    fn stop_joins() {
        let (h, reads, _f) = open(None);
        std::thread::sleep(Duration::from_millis(60));
        h.stop();
        let n = reads.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(n, reads.load(Ordering::SeqCst), "no reads after stop");
    }

    #[test]
    fn pause_stops_pulling_and_resume_continues() {
        let (h, reads, _f) = open(None);
        std::thread::sleep(Duration::from_millis(100));
        h.pause().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let n = reads.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(n, reads.load(Ordering::SeqCst));
        h.resume().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let m = reads.load(Ordering::SeqCst);
        h.stop();
        assert!(m > n && m - n < 10, "resumed without a burst: {n} -> {m}");
    }
}
