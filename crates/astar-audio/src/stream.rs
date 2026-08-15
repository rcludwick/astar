// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Stream traits and cpal-backed implementation.
//!
//! # Threading model
//!
//! cpal's `Stream` is `!Send` on several host backends (notably `CoreAudio`
//! pre-0.16 and ALSA in some configurations). To still expose a `Send`
//! [`StreamHandle`] — which the public [`AudioBackend`](crate::AudioBackend)
//! API requires so consumers can stash handles in `Arc<Mutex<...>>` —
//! we own the cpal stream on a dedicated thread and communicate via
//! [`std::sync::mpsc`] channels. The thread parks on the channel; on each
//! control message (`Pause`, `Resume`, `Stop`) it pokes the cpal stream
//! and either continues waiting or exits.
//!
//! The audio callback itself still runs on cpal's audio thread — we never
//! touch the channel from inside the callback.

// Audio code does lots of intentional numeric casts (sample format
// conversion, frame-count math). These are sound for the value ranges
// involved and the alternatives (try_from + panic / log) are noisier than
// they're worth on the hot path.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::cast_possible_wrap,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::float_cmp
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SampleRate, StreamConfig as CpalConfig};

use crate::device::{DeviceId, DeviceInfo, Direction};
use crate::error::AudioError;
use crate::meter::peak;
use crate::resample::AntiAliasResampler;

/// Stream configuration the caller asks for. The backend negotiates
/// against device capabilities and resamples if needed.
#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    /// Target sample rate at the trait boundary (e.g. 8000 for IAX2 voice).
    pub sample_rate: u32,
    /// Channel count at the trait boundary. Typically 1 for voice.
    pub channels: u16,
    /// Requested buffer size in milliseconds. The backend uses this as a
    /// hint when configuring cpal; the actual buffer size may differ.
    pub buffer_ms: u32,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            sample_rate: 8_000,
            channels: 1,
            buffer_ms: 20,
        }
    }
}

/// Sink that the input callback drains samples *into*.
///
/// Called from cpal's audio thread; implementations must not block.
pub trait InputSink: Send {
    /// Receive a slice of samples already converted to the requested
    /// [`StreamConfig::sample_rate`] / channel count. `meter` is the peak
    /// amplitude over the original device-rate callback buffer, in
    /// `0.0..=1.0`.
    fn write(&mut self, samples: &[f32], meter: f32);
}

/// Source the output callback pulls samples *from*.
///
/// Called from cpal's audio thread; implementations must not block.
pub trait OutputSource: Send {
    /// Fill `out` with up to `out.len()` samples at the requested
    /// [`StreamConfig::sample_rate`] / channel count. Returns the number
    /// of samples actually written; the caller zero-fills the trailing
    /// slots so partial fills are safe.
    fn read(&mut self, out: &mut [f32]) -> usize;

    /// Called once per cpal callback after rendering, with the post-mix
    /// peak amplitude in `0.0..=1.0`. Default impl discards.
    fn meter(&mut self, _level: f32) {}
}

/// Opaque handle to a running stream. Drop to stop; or call `stop()` /
/// `pause()` / `resume()` explicitly.
pub trait StreamHandle: Send {
    /// Stop the stream and join its worker thread. Consumes the handle.
    fn stop(self: Box<Self>);
    /// Pause sample delivery (cpal `pause()`).
    fn pause(&self) -> Result<(), AudioError>;
    /// Resume sample delivery (cpal `play()`).
    fn resume(&self) -> Result<(), AudioError>;
}

/// Control message sent from the public `StreamHandle` methods to the
/// stream-owning thread.
enum Ctl {
    Pause,
    Resume,
    Stop,
}

/// Reply channel: the stream thread sends back the outcome of a control op.
type CtlReply = Sender<Result<(), AudioError>>;

/// How long to wait for the stream thread to answer a control message
/// (iax-8d21). Pausing or resuming a HEALTHY `CoreAudio` stream is
/// sub-millisecond; this bound exists only for the case where the device has
/// been physically removed and the call never returns at all.
const CTL_REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to wait for the stream thread to finish shutting down — pausing
/// the stream and then dropping it, both `CoreAudio` calls that can wedge on
/// a removed device (iax-8d21).
///
/// Kept to one second deliberately: a station tears down an input AND an
/// output stream, each of which can burn both this and `CTL_REPLY_TIMEOUT`,
/// so the worst-case Quit with a wedged device is ~4s. A healthy stop is
/// sub-millisecond, so this is still three orders of magnitude of headroom.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to wait for a newly-spawned stream thread to report whether it
/// opened the device (iax-8d21). Generous, because a first open of a real
/// device takes a noticeable fraction of a second; bounded, because a device
/// that vanished mid-open never answers at all — and this wait runs on the
/// CALLER's thread, which for astar is the dial.
const BUILD_TIMEOUT: Duration = Duration::from_secs(5);

/// Wait for a control reply, bounded by `timeout`.
///
/// A free function so the policy is unit-testable without a sound card: the
/// wedged-device case is precisely the one no test can provoke through cpal.
fn await_reply(rx: &Receiver<Result<(), AudioError>>, timeout: Duration) -> Result<(), AudioError> {
    match rx.recv_timeout(timeout) {
        Ok(res) => res,
        Err(RecvTimeoutError::Timeout) => Err(AudioError::StreamControl(
            "audio device did not respond (disabled) — it was most likely unplugged".into(),
        )),
        Err(RecvTimeoutError::Disconnected) => {
            Err(AudioError::StreamControl("reply channel closed".into()))
        }
    }
}

/// Join `handle`, but only once the thread signals it has finished, and only
/// if it does so within `timeout`. Returns `true` when it was joined.
///
/// On timeout the handle is DROPPED WITHOUT JOINING: the thread is detached
/// and left wherever `CoreAudio` stranded it, leaking that thread and its
/// `cpal::Stream`.
///
/// The leak is the deliberate trade (iax-8d21). The thread is blocked inside
/// the OS on a device that no longer exists — nothing this process does frees
/// it sooner, and joining it is exactly what froze the application, including
/// its Quit path, since `Station`'s `Drop` chain runs through here. A leaked
/// thread on a hot-unplugged device is recoverable; an app that cannot be
/// quit is not. In practice the stranded call returns by itself once the OS
/// finishes tearing the device down, and the thread then exits on its own.
fn join_before(handle: JoinHandle<()>, done: &Receiver<()>, timeout: Duration) -> bool {
    if done.recv_timeout(timeout).is_ok() {
        // Signalled: this join is guaranteed not to block.
        let _ = handle.join();
        return true;
    }
    tracing::error!(
        "audio stream thread did not shut down within {timeout:?} — detaching it. The device was \
         most likely unplugged while in use; this leaks one thread and its cpal stream rather \
         than hanging the caller (iax-8d21)."
    );
    drop(handle);
    false
}

/// Concrete `StreamHandle` over a dedicated stream-owning thread.
struct CpalStreamHandle {
    tx: Sender<(Ctl, CtlReply)>,
    join: Option<JoinHandle<()>>,
    /// Signalled by the stream thread once it has paused AND dropped the
    /// `cpal::Stream` — i.e. once joining it cannot block.
    done: Receiver<()>,
}

impl CpalStreamHandle {
    fn send(&self, ctl: Ctl) -> Result<(), AudioError> {
        let (reply_tx, reply_rx) = channel::<Result<(), AudioError>>();
        self.tx
            .send((ctl, reply_tx))
            .map_err(|_| AudioError::StreamControl("stream thread gone".into()))?;
        await_reply(&reply_rx, CTL_REPLY_TIMEOUT)
    }

    /// Ask the thread to stop and wait for it, bounded. Shared by `stop()`
    /// and `Drop` so both behave identically when the device is wedged.
    fn shutdown(&mut self) {
        let _ = self.send(Ctl::Stop);
        if let Some(j) = self.join.take() {
            join_before(j, &self.done, SHUTDOWN_TIMEOUT);
        }
    }
}

impl StreamHandle for CpalStreamHandle {
    fn stop(mut self: Box<Self>) {
        self.shutdown();
    }

    fn pause(&self) -> Result<(), AudioError> {
        self.send(Ctl::Pause)
    }

    fn resume(&self) -> Result<(), AudioError> {
        self.send(Ctl::Resume)
    }
}

impl Drop for CpalStreamHandle {
    fn drop(&mut self) {
        // A handle dropped without `stop` still has to tell the thread to
        // shut down, or the cpal Stream leaks — bounded exactly as `stop` is,
        // since this Drop runs on `Station`'s teardown path and an unbounded
        // wait here is an application that cannot be quit.
        self.shutdown();
    }
}

/// Top-level audio backend trait.
///
/// Implementations are object-safe: callers can hold `Box<dyn AudioBackend>`
/// (e.g. to inject a mock during tests).
pub trait AudioBackend: Send + Sync {
    /// Enumerate all input and output devices on the host.
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError>;

    /// Default input device, if the host reports one.
    fn default_input(&self) -> Option<DeviceInfo>;

    /// Default output device, if the host reports one.
    fn default_output(&self) -> Option<DeviceInfo>;

    /// Open an input (capture) stream on `device` with `config`, piping
    /// samples into `sink`. The returned handle keeps the stream alive
    /// until dropped or `stop()`d.
    ///
    /// `overruns` is a shared cumulative counter the capture thread bumps each
    /// time it detects a dropped input buffer (a hole in the captured PCM — the
    /// lead suspect for choppy TX, iax-5530/iax-9e55). A plain `u64` health
    /// counter, credential-free; surfaced in the snapshot as `tx_capture_overruns`.
    fn open_input(
        &self,
        device: &DeviceInfo,
        config: StreamConfig,
        sink: Box<dyn InputSink>,
        overruns: Arc<AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError>;

    /// Open an output (playback) stream on `device` with `config`,
    /// pulling samples from `source`.
    fn open_output(
        &self,
        device: &DeviceInfo,
        config: StreamConfig,
        source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError>;
}

/// cpal-backed implementation of [`AudioBackend`].
///
/// Constructed with [`CpalBackend::new`]; uses cpal's default host.
pub struct CpalBackend {
    host: cpal::Host,
}

impl CpalBackend {
    /// Build a backend using cpal's default host (`CoreAudio` on macOS,
    /// WASAPI on Windows, ALSA/`PipeWire` on Linux).
    #[must_use]
    pub fn new() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }

    fn enumerate(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        let mut out = Vec::new();

        // Inputs.
        let inputs = self
            .host
            .input_devices()
            .map_err(|e| AudioError::Enumeration(e.to_string()))?;
        for dev in inputs {
            let name = dev.name().unwrap_or_else(|_| "<unknown>".into());
            let (channels, rates) = match dev.default_input_config() {
                Ok(cfg) => (cfg.channels(), vec![cfg.sample_rate().0]),
                Err(_) => (1, Vec::new()),
            };
            out.push(DeviceInfo {
                id: DeviceId::new(format!("in:{name}")),
                name,
                direction: Direction::Input,
                channels,
                native_sample_rates: rates,
            });
        }

        // Outputs.
        let outputs = self
            .host
            .output_devices()
            .map_err(|e| AudioError::Enumeration(e.to_string()))?;
        for dev in outputs {
            let name = dev.name().unwrap_or_else(|_| "<unknown>".into());
            let (channels, rates) = match dev.default_output_config() {
                Ok(cfg) => (cfg.channels(), vec![cfg.sample_rate().0]),
                Err(_) => (2, Vec::new()),
            };
            out.push(DeviceInfo {
                id: DeviceId::new(format!("out:{name}")),
                name,
                direction: Direction::Output,
                channels,
                native_sample_rates: rates,
            });
        }

        Ok(out)
    }

    /// Locate a cpal device by `DeviceId` (the "in:<name>" / "out:<name>"
    /// strings produced by `enumerate`).
    fn find_device(&self, id: &DeviceId) -> Result<(cpal::Device, bool), AudioError> {
        let s = id.as_str();
        let (is_input, name) = if let Some(rest) = s.strip_prefix("in:") {
            (true, rest.to_string())
        } else if let Some(rest) = s.strip_prefix("out:") {
            (false, rest.to_string())
        } else {
            return Err(AudioError::DeviceNotFound(s.to_string()));
        };

        let iter: Box<dyn Iterator<Item = cpal::Device>> = if is_input {
            Box::new(
                self.host
                    .input_devices()
                    .map_err(|e| AudioError::Enumeration(e.to_string()))?,
            )
        } else {
            Box::new(
                self.host
                    .output_devices()
                    .map_err(|e| AudioError::Enumeration(e.to_string()))?,
            )
        };
        for dev in iter {
            if dev.name().as_deref().unwrap_or("") == name {
                return Ok((dev, is_input));
            }
        }
        Err(AudioError::DeviceNotFound(name))
    }
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioBackend for CpalBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        self.enumerate()
    }

    fn default_input(&self) -> Option<DeviceInfo> {
        let dev = self.host.default_input_device()?;
        let name = dev.name().unwrap_or_else(|_| "<unknown>".into());
        let cfg = dev.default_input_config().ok();
        Some(DeviceInfo {
            id: DeviceId::new(format!("in:{name}")),
            name,
            direction: Direction::Input,
            channels: cfg
                .as_ref()
                .map_or(1, cpal::SupportedStreamConfig::channels),
            native_sample_rates: cfg.map(|c| vec![c.sample_rate().0]).unwrap_or_default(),
        })
    }

    fn default_output(&self) -> Option<DeviceInfo> {
        let dev = self.host.default_output_device()?;
        let name = dev.name().unwrap_or_else(|_| "<unknown>".into());
        let cfg = dev.default_output_config().ok();
        Some(DeviceInfo {
            id: DeviceId::new(format!("out:{name}")),
            name,
            direction: Direction::Output,
            channels: cfg
                .as_ref()
                .map_or(2, cpal::SupportedStreamConfig::channels),
            native_sample_rates: cfg.map(|c| vec![c.sample_rate().0]).unwrap_or_default(),
        })
    }

    fn open_input(
        &self,
        device: &DeviceInfo,
        config: StreamConfig,
        sink: Box<dyn InputSink>,
        overruns: Arc<AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        let (dev, is_input) = self.find_device(&device.id)?;
        if !is_input {
            return Err(AudioError::DeviceNotFound(format!(
                "{} is not an input device",
                device.name
            )));
        }

        let supported = dev
            .default_input_config()
            .map_err(|e| AudioError::BuildStream(e.to_string()))?;
        let device_rate = supported.sample_rate().0;
        let device_channels = supported.channels();
        let sample_format = supported.sample_format();

        let target_rate = config.sample_rate;
        let target_channels = config.channels;

        // Build cpal stream config at the device's native rate; we resample
        // inside the callback if needed.
        let cpal_cfg = CpalConfig {
            channels: device_channels,
            sample_rate: SampleRate(device_rate),
            buffer_size: BufferSize::Default,
        };

        spawn_input_stream(
            dev,
            cpal_cfg,
            sample_format,
            device_rate,
            device_channels,
            target_rate,
            target_channels,
            sink,
            overruns,
        )
    }

    fn open_output(
        &self,
        device: &DeviceInfo,
        config: StreamConfig,
        source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        let (dev, is_input) = self.find_device(&device.id)?;
        if is_input {
            return Err(AudioError::DeviceNotFound(format!(
                "{} is not an output device",
                device.name
            )));
        }
        let supported = dev
            .default_output_config()
            .map_err(|e| AudioError::BuildStream(e.to_string()))?;
        let device_rate = supported.sample_rate().0;
        let device_channels = supported.channels();
        let sample_format = supported.sample_format();

        let target_rate = config.sample_rate;
        let target_channels = config.channels;

        let cpal_cfg = CpalConfig {
            channels: device_channels,
            sample_rate: SampleRate(device_rate),
            buffer_size: BufferSize::Default,
        };

        spawn_output_stream(
            dev,
            cpal_cfg,
            sample_format,
            device_rate,
            device_channels,
            target_rate,
            target_channels,
            source,
        )
    }
}

/// Spawn the input-stream owning thread and build the cpal stream on it.
#[allow(clippy::too_many_arguments)]
fn spawn_input_stream(
    dev: cpal::Device,
    cpal_cfg: CpalConfig,
    sample_format: SampleFormat,
    device_rate: u32,
    device_channels: u16,
    target_rate: u32,
    target_channels: u16,
    mut sink: Box<dyn InputSink>,
    overruns: Arc<AtomicU64>,
) -> Result<Box<dyn StreamHandle>, AudioError> {
    let (tx, rx) = channel::<(Ctl, CtlReply)>();
    let (init_tx, init_rx) = channel::<Result<(), AudioError>>();
    // Signalled by the thread once the cpal stream is paused AND dropped, so
    // `join_before` knows when joining is safe (iax-8d21).
    let (done_tx, done_rx) = channel::<()>();

    let join = std::thread::Builder::new()
        .name("astar-audio-input".into())
        .spawn(move || {
            // Build the resampler on the audio thread (rubato's resamplers
            // aren't Send on all configs; keeping it thread-local sidesteps
            // that).
            let mut resampler = if device_rate == target_rate {
                None
            } else {
                match AntiAliasResampler::new(device_rate, target_rate) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                }
            };

            // Scratch buffer for mono-downmixed device samples (before
            // resampling). We size lazily on first callback.
            let mut mono = Vec::<f32>::new();
            // Output buffer drained from the resampler each callback.
            let mut resampled = Vec::<f32>::new();
            // TX instrumentation (iax-5530): watch for dropped capture buffers,
            // counting each into the shared `overruns` cell (iax-9e55).
            let mut gap_watch = CaptureGapWatch::new(device_rate, overruns);

            let err_fn = |e| tracing::warn!(target: "astar_audio", "cpal input stream error: {e}");

            let build = || -> Result<cpal::Stream, AudioError> {
                match sample_format {
                    SampleFormat::F32 => dev
                        .build_input_stream(
                            &cpal_cfg,
                            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                                gap_watch.check(info, data.len() / device_channels.max(1) as usize);
                                process_input(
                                    data,
                                    device_channels,
                                    target_channels,
                                    resampler.as_mut(),
                                    &mut mono,
                                    &mut resampled,
                                    sink.as_mut(),
                                );
                            },
                            err_fn,
                            None,
                        )
                        .map_err(|e| AudioError::BuildStream(e.to_string())),
                    SampleFormat::I16 => dev
                        .build_input_stream(
                            &cpal_cfg,
                            move |data: &[i16], info: &cpal::InputCallbackInfo| {
                                gap_watch.check(info, data.len() / device_channels.max(1) as usize);
                                // Convert i16 -> f32 into a scratch then process.
                                mono.clear();
                                for &s in data {
                                    mono.push(f32::from(s) / f32::from(i16::MAX));
                                }
                                let m = mono.clone();
                                process_input(
                                    &m,
                                    device_channels,
                                    target_channels,
                                    resampler.as_mut(),
                                    &mut mono,
                                    &mut resampled,
                                    sink.as_mut(),
                                );
                            },
                            err_fn,
                            None,
                        )
                        .map_err(|e| AudioError::BuildStream(e.to_string())),
                    SampleFormat::U16 => dev
                        .build_input_stream(
                            &cpal_cfg,
                            move |data: &[u16], info: &cpal::InputCallbackInfo| {
                                gap_watch.check(info, data.len() / device_channels.max(1) as usize);
                                mono.clear();
                                for &s in data {
                                    let v =
                                        (f32::from(s) - f32::from(i16::MAX)) / f32::from(i16::MAX);
                                    mono.push(v);
                                }
                                let m = mono.clone();
                                process_input(
                                    &m,
                                    device_channels,
                                    target_channels,
                                    resampler.as_mut(),
                                    &mut mono,
                                    &mut resampled,
                                    sink.as_mut(),
                                );
                            },
                            err_fn,
                            None,
                        )
                        .map_err(|e| AudioError::BuildStream(e.to_string())),
                    other => Err(AudioError::BuildStream(format!(
                        "unsupported sample format {other:?}"
                    ))),
                }
            };

            let stream = match build() {
                Ok(s) => s,
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };
            if let Err(e) = stream.play() {
                let _ = init_tx.send(Err(AudioError::StreamControl(e.to_string())));
                return;
            }
            let _ = init_tx.send(Ok(()));

            // Service control messages until told to stop.
            for (ctl, reply) in rx {
                let res = match ctl {
                    Ctl::Pause => stream
                        .pause()
                        .map_err(|e| AudioError::StreamControl(e.to_string())),
                    Ctl::Resume => stream
                        .play()
                        .map_err(|e| AudioError::StreamControl(e.to_string())),
                    Ctl::Stop => {
                        // Pause first: on macOS/CoreAudio, dropping a cpal
                        // stream alone can leave the device running (it keeps
                        // capturing/playing). Pausing stops the device before
                        // the stream is dropped below.
                        let _ = stream.pause();
                        Ok(())
                    }
                };
                let stopping = matches!(ctl, Ctl::Stop);
                let _ = reply.send(res);
                if stopping {
                    break;
                }
            }
            // Drop the cpal stream EXPLICITLY, before signalling: on a
            // removed device this drop is itself a CoreAudio call that can
            // block, and the whole point of `done` is that it is only sent
            // once nothing is left that could (iax-8d21).
            drop(stream);
            let _ = done_tx.send(());
        })
        .map_err(|e| AudioError::BuildStream(format!("thread spawn: {e}")))?;

    // Wait for init result.
    // Bounded (iax-8d21): a device that disappeared mid-open never reports
    // back, and this wait is on the caller's thread — unbounded, it is a dial
    // that never returns and a Quit that never completes.
    match init_rx.recv_timeout(BUILD_TIMEOUT) {
        Ok(Ok(())) => Ok(Box::new(CpalStreamHandle {
            tx,
            join: Some(join),
            done: done_rx,
        })),
        Ok(Err(e)) => {
            join_before(join, &done_rx, SHUTDOWN_TIMEOUT);
            Err(e)
        }
        Err(RecvTimeoutError::Timeout) => {
            join_before(join, &done_rx, SHUTDOWN_TIMEOUT);
            Err(AudioError::BuildStream(
                "audio device did not open within the timeout (disabled) — it was most likely \
                 unplugged"
                    .into(),
            ))
        }
        Err(RecvTimeoutError::Disconnected) => {
            join_before(join, &done_rx, SHUTDOWN_TIMEOUT);
            Err(AudioError::BuildStream("init channel closed".into()))
        }
    }
}

/// Detects dropped capture buffers (overruns) by watching the gap between
/// consecutive callbacks' capture timestamps (iax-5530). When the OS drops an
/// input buffer under load, the next callback's capture instant jumps further
/// ahead than the previous buffer's duration — a real hole in the captured PCM,
/// the lead suspect for intermittent choppy TX. Logs a warning on
/// `target = "astar_iax::tx_audio"` only when a drop is detected (no per-callback
/// overhead beyond one instant compare).
struct CaptureGapWatch {
    rate: u32,
    prev_capture: Option<cpal::StreamInstant>,
    prev_frames: usize,
    /// Shared cumulative overrun counter, bumped once per detected drop and
    /// surfaced in the snapshot as `tx_capture_overruns` (iax-9e55). A plain
    /// `u64` health counter, credential-free.
    overruns: Arc<AtomicU64>,
}

impl CaptureGapWatch {
    fn new(rate: u32, overruns: Arc<AtomicU64>) -> Self {
        Self {
            rate,
            prev_capture: None,
            prev_frames: 0,
            overruns,
        }
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn check(&mut self, info: &cpal::InputCallbackInfo, frames: usize) {
        let cap = info.timestamp().capture;
        // Swap in this callback's state first, so every early-return path leaves
        // the watch ready for the next callback.
        let prev_frames = std::mem::replace(&mut self.prev_frames, frames);
        let Some(prev) = self.prev_capture.replace(cap) else {
            return;
        };
        if prev_frames == 0 {
            return;
        }
        let Some(gap) = cap.duration_since(&prev) else {
            return;
        };
        // Expected gap = the previous buffer's duration; excess ⇒ dropped buffers.
        let expected = Duration::from_secs_f64(prev_frames as f64 / f64::from(self.rate.max(1)));
        let excess = gap.saturating_sub(expected);
        // A dropped buffer adds ~one buffer's worth (>= ~5 ms at 48 kHz / 256
        // frames); 5 ms slack ignores normal scheduling jitter.
        if excess >= Duration::from_millis(5) {
            self.overruns.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                target: "astar_iax::tx_audio",
                dropped_ms = excess.as_millis() as u64,
                "capture overrun: input buffer(s) dropped (gap in mic PCM)"
            );
        }
    }
}

/// Process one cpal input callback: downmix to mono, meter, optionally
/// resample, and deliver to the sink.
fn process_input(
    data: &[f32],
    device_channels: u16,
    target_channels: u16,
    resampler: Option<&mut AntiAliasResampler>,
    mono: &mut Vec<f32>,
    resampled: &mut Vec<f32>,
    sink: &mut dyn InputSink,
) {
    let meter = peak(data);

    // Downmix to mono (target_channels == 1 in the IAX2 case). If the
    // caller asked for more than 1 channel we just take channel 0 as a
    // simple stub — voice doesn't need anything fancier here.
    mono.clear();
    let dc = device_channels.max(1) as usize;
    if dc == 1 {
        mono.extend_from_slice(data);
    } else {
        // Average across device channels for mono output.
        for frame in data.chunks_exact(dc) {
            let sum: f32 = frame.iter().sum();
            mono.push(sum / dc as f32);
        }
    }

    let samples_to_deliver: &[f32] = if let Some(r) = resampler {
        let _ = r.push(mono);
        resampled.clear();
        // Drain everything currently buffered.
        let avail = r.drain_all();
        resampled.extend_from_slice(&avail);
        resampled
    } else {
        mono
    };

    if target_channels <= 1 {
        sink.write(samples_to_deliver, meter);
    } else {
        // Duplicate the mono stream across `target_channels`. Rare path.
        let mut buf = Vec::with_capacity(samples_to_deliver.len() * target_channels as usize);
        for &s in samples_to_deliver {
            for _ in 0..target_channels {
                buf.push(s);
            }
        }
        sink.write(&buf, meter);
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_output_stream(
    dev: cpal::Device,
    cpal_cfg: CpalConfig,
    sample_format: SampleFormat,
    device_rate: u32,
    device_channels: u16,
    target_rate: u32,
    target_channels: u16,
    mut source: Box<dyn OutputSource>,
) -> Result<Box<dyn StreamHandle>, AudioError> {
    let (tx, rx) = channel::<(Ctl, CtlReply)>();
    let (init_tx, init_rx) = channel::<Result<(), AudioError>>();
    // Signalled by the thread once the cpal stream is paused AND dropped, so
    // `join_before` knows when joining is safe (iax-8d21).
    let (done_tx, done_rx) = channel::<()>();

    let join = std::thread::Builder::new()
        .name("astar-audio-output".into())
        .spawn(move || {
            // Output direction: source emits target_rate; we resample up
            // to device_rate.
            let mut resampler = if device_rate == target_rate {
                None
            } else {
                match AntiAliasResampler::new(target_rate, device_rate) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                }
            };

            // Scratch buffer for mono samples pulled from `source` at
            // target_rate.
            let mut src_buf = Vec::<f32>::new();
            // Scratch buffer for samples after resampling, at device_rate.
            let mut up_buf = Vec::<f32>::new();
            // Residual: device-rate mono samples that we couldn't fit into
            // the previous callback (because device_channels > 1 or because
            // the resampler produced more than we needed). Drained first.
            let mut residual = std::collections::VecDeque::<f32>::new();

            let err_fn = |e| tracing::warn!(target: "astar_audio", "cpal output stream error: {e}");

            let build = || -> Result<cpal::Stream, AudioError> {
                match sample_format {
                    SampleFormat::F32 => dev
                        .build_output_stream(
                            &cpal_cfg,
                            move |data: &mut [f32], _| {
                                fill_output(
                                    data,
                                    device_channels,
                                    target_channels,
                                    device_rate,
                                    target_rate,
                                    resampler.as_mut(),
                                    &mut src_buf,
                                    &mut up_buf,
                                    &mut residual,
                                    source.as_mut(),
                                );
                            },
                            err_fn,
                            None,
                        )
                        .map_err(|e| AudioError::BuildStream(e.to_string())),
                    SampleFormat::I16 => dev
                        .build_output_stream(
                            &cpal_cfg,
                            move |data: &mut [i16], _| {
                                let mut tmp = vec![0.0_f32; data.len()];
                                fill_output(
                                    &mut tmp,
                                    device_channels,
                                    target_channels,
                                    device_rate,
                                    target_rate,
                                    resampler.as_mut(),
                                    &mut src_buf,
                                    &mut up_buf,
                                    &mut residual,
                                    source.as_mut(),
                                );
                                for (d, s) in data.iter_mut().zip(tmp.iter()) {
                                    *d = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                                }
                            },
                            err_fn,
                            None,
                        )
                        .map_err(|e| AudioError::BuildStream(e.to_string())),
                    other => Err(AudioError::BuildStream(format!(
                        "unsupported sample format {other:?}"
                    ))),
                }
            };

            let stream = match build() {
                Ok(s) => s,
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };
            if let Err(e) = stream.play() {
                let _ = init_tx.send(Err(AudioError::StreamControl(e.to_string())));
                return;
            }
            let _ = init_tx.send(Ok(()));

            for (ctl, reply) in rx {
                let res = match ctl {
                    Ctl::Pause => stream
                        .pause()
                        .map_err(|e| AudioError::StreamControl(e.to_string())),
                    Ctl::Resume => stream
                        .play()
                        .map_err(|e| AudioError::StreamControl(e.to_string())),
                    Ctl::Stop => {
                        // Pause first: on macOS/CoreAudio, dropping a cpal
                        // stream alone can leave the device running (it keeps
                        // capturing/playing). Pausing stops the device before
                        // the stream is dropped below.
                        let _ = stream.pause();
                        Ok(())
                    }
                };
                let stopping = matches!(ctl, Ctl::Stop);
                let _ = reply.send(res);
                if stopping {
                    break;
                }
            }
            // Explicit, and before signalling — see the input path's note.
            drop(stream);
            let _ = done_tx.send(());
        })
        .map_err(|e| AudioError::BuildStream(format!("thread spawn: {e}")))?;

    // Bounded (iax-8d21): a device that disappeared mid-open never reports
    // back, and this wait is on the caller's thread — unbounded, it is a dial
    // that never returns and a Quit that never completes.
    match init_rx.recv_timeout(BUILD_TIMEOUT) {
        Ok(Ok(())) => Ok(Box::new(CpalStreamHandle {
            tx,
            join: Some(join),
            done: done_rx,
        })),
        Ok(Err(e)) => {
            join_before(join, &done_rx, SHUTDOWN_TIMEOUT);
            Err(e)
        }
        Err(RecvTimeoutError::Timeout) => {
            join_before(join, &done_rx, SHUTDOWN_TIMEOUT);
            Err(AudioError::BuildStream(
                "audio device did not open within the timeout (disabled) — it was most likely \
                 unplugged"
                    .into(),
            ))
        }
        Err(RecvTimeoutError::Disconnected) => {
            join_before(join, &done_rx, SHUTDOWN_TIMEOUT);
            Err(AudioError::BuildStream("init channel closed".into()))
        }
    }
}

/// Fill a cpal output buffer: pull mono samples at `target_rate`, resample
/// up to `device_rate`, expand across device channels, meter, and write.
#[allow(clippy::too_many_arguments)]
fn fill_output(
    out: &mut [f32],
    device_channels: u16,
    _target_channels: u16,
    device_rate: u32,
    target_rate: u32,
    mut resampler: Option<&mut AntiAliasResampler>,
    src_buf: &mut Vec<f32>,
    _up_buf: &mut Vec<f32>,
    residual: &mut std::collections::VecDeque<f32>,
    source: &mut dyn OutputSource,
) {
    let dc = device_channels.max(1) as usize;
    let frames_needed = out.len() / dc;

    // Drain residual mono frames first.
    let mut produced_mono = Vec::<f32>::with_capacity(frames_needed);
    while produced_mono.len() < frames_needed {
        if let Some(s) = residual.pop_front() {
            produced_mono.push(s);
            continue;
        }
        // Need more mono samples. Estimate how many source samples we need
        // at target_rate to yield `frames_needed - produced_mono.len()`
        // device-rate samples. Conservative: ask for enough to cover one
        // chunk plus headroom.
        let remaining_device = frames_needed - produced_mono.len();
        let request_src = if resampler.is_some() {
            // src_samples = device_samples * target_rate / device_rate
            ((remaining_device as u64 * u64::from(target_rate)) / u64::from(device_rate).max(1))
                as usize
                + 512
        } else {
            remaining_device
        };
        src_buf.clear();
        src_buf.resize(request_src, 0.0);
        let n = source.read(src_buf);
        // Feed downstream ONLY the `n` samples actually read — never the
        // zero padding. Pushing the padded tail stitched fabricated silence
        // between real network chunks (voice trickles in 20 ms pieces while
        // the request carries +512 headroom), which made received audio
        // sound robotic (iax-0b3f). True underruns pad the OUTPUT below.
        if let Some(r) = resampler.as_deref_mut() {
            let _ = r.push(&src_buf[..n]);
            let avail = r.drain_all();
            for s in avail {
                if produced_mono.len() < frames_needed {
                    produced_mono.push(s);
                } else {
                    residual.push_back(s);
                }
            }
        } else {
            for &s in &src_buf[..n] {
                if produced_mono.len() < frames_needed {
                    produced_mono.push(s);
                } else {
                    residual.push_back(s);
                }
            }
        }
        // Source dry (n == 0): a genuine underrun. Pad the OUTPUT tail with
        // silence and stop — this is the only place silence may enter, and it
        // never lands between real samples. While n > 0 keep looping: the
        // resampler may be holding a partial chunk (< 256 buffered) that the
        // next read tops up, so bailing early here would fabricate a silent
        // callback with real audio still queued. Termination is guaranteed —
        // every n > 0 read drains the source toward n == 0.
        if n == 0 {
            produced_mono.resize(frames_needed, 0.0);
            break;
        }
    }

    // Expand mono → device_channels into `out`.
    for (i, frame) in out.chunks_exact_mut(dc).enumerate() {
        let s = produced_mono.get(i).copied().unwrap_or(0.0);
        for slot in frame.iter_mut() {
            *slot = s;
        }
    }
    // Any leftover slots (when out.len() isn't a multiple of dc) → silence.
    let used = (out.len() / dc) * dc;
    for v in out.iter_mut().skip(used) {
        *v = 0.0;
    }

    let level = peak(out);
    source.meter(level);
}

/// Test-only backend + control surface mirroring `astar-iax`'s
/// `test_support::NullBackend`, exposed to the in-crate `router` tests
/// (iax-42e9 phase 1) AND the crate's integration tests. Returns
/// `in:test` / `out:test` devices and no-op stream handles, and holds the
/// most-recently opened input sink so a test can drive it via `push_mic`.
/// Exposed publicly under the `test-backend` feature so `tests/*.rs` (which
/// compile against the crate as an external dependency) can construct a
/// hardware-free router.
#[cfg(any(test, feature = "test-backend"))]
pub mod test_support_router {
    use std::sync::{Arc, Mutex};

    use crate::device::{DeviceId, DeviceInfo, Direction};
    use crate::error::AudioError;
    use crate::stream::{AudioBackend, InputSink, OutputSource, StreamConfig, StreamHandle};

    #[derive(Default)]
    struct Shared {
        sink: Option<Box<dyn InputSink>>,
    }

    /// Test backend: returns `in:test`/`out:test` and stashes the opened sink.
    pub struct NullBackend(Arc<Mutex<Shared>>);

    /// Cloneable control surface that survives the backend being moved into
    /// the router; mirrors `astar-iax`'s `NullControls`. `push_mic` drives the
    /// stashed sink, which is how the router's input path is exercised once the
    /// backend is owned by the router. Unit-test-only (the integration tests
    /// don't drive the sink directly).
    #[cfg(test)]
    #[derive(Clone)]
    pub(crate) struct NullControls(Arc<Mutex<Shared>>);

    impl NullBackend {
        /// Construct the backend plus a cloneable control handle.
        #[must_use]
        pub fn new() -> Self {
            Self(Arc::new(Mutex::new(Shared::default())))
        }
        /// Construct the backend plus a cloneable control handle so a test can
        /// `push_mic` after the backend has been moved into the router.
        #[cfg(test)]
        pub(crate) fn with_controls() -> (Self, NullControls) {
            let s = Arc::new(Mutex::new(Shared::default()));
            (Self(Arc::clone(&s)), NullControls(s))
        }
    }

    impl Default for NullBackend {
        fn default() -> Self {
            Self::new()
        }
    }

    #[cfg(test)]
    impl NullControls {
        /// Drive the router's most-recently opened input sink with `samples`,
        /// as the cpal capture callback would.
        pub(crate) fn push_mic(&self, samples: &[f32]) {
            let mut g = self.0.lock().unwrap();
            if let Some(mut sink) = g.sink.take() {
                drop(g);
                sink.write(samples, 0.0);
                self.0.lock().unwrap().sink = Some(sink);
            }
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
                dev(Direction::Input, "in:test"),
                dev(Direction::Output, "out:test"),
            ])
        }
        fn default_input(&self) -> Option<DeviceInfo> {
            Some(dev(Direction::Input, "in:test"))
        }
        fn default_output(&self) -> Option<DeviceInfo> {
            Some(dev(Direction::Output, "out:test"))
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

    /// Multi-device test backend: returns two inputs (`in:a`, `in:b`) and one
    /// shared output (`out:shared`) plus no-op stream handles, so the N-lane
    /// router (iax-42e9 phase 2) can open two mics that share one speaker.
    /// Unit-test-only.
    #[cfg(test)]
    pub(crate) struct MultiNullBackend;

    #[cfg(test)]
    impl MultiNullBackend {
        pub(crate) fn new() -> Self {
            Self
        }
    }

    #[cfg(test)]
    impl AudioBackend for MultiNullBackend {
        fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            Ok(vec![
                dev(Direction::Input, "in:a"),
                dev(Direction::Input, "in:b"),
                dev(Direction::Output, "out:shared"),
            ])
        }
        fn default_input(&self) -> Option<DeviceInfo> {
            Some(dev(Direction::Input, "in:a"))
        }
        fn default_output(&self) -> Option<DeviceInfo> {
            Some(dev(Direction::Output, "out:shared"))
        }
        fn open_input(
            &self,
            _d: &DeviceInfo,
            _c: StreamConfig,
            _sink: Box<dyn InputSink>,
            _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
        ) -> Result<Box<dyn StreamHandle>, AudioError> {
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
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Confirm trait objects are constructible — i.e., the traits are
    /// dyn-compatible / object-safe.
    #[test]
    fn traits_are_object_safe() {
        struct NullSink;
        impl InputSink for NullSink {
            fn write(&mut self, _samples: &[f32], _meter: f32) {}
        }
        struct NullSrc;
        impl OutputSource for NullSrc {
            fn read(&mut self, out: &mut [f32]) -> usize {
                for v in out.iter_mut() {
                    *v = 0.0;
                }
                out.len()
            }
        }
        let _: Box<dyn InputSink> = Box::new(NullSink);
        let _: Box<dyn OutputSource> = Box::new(NullSrc);
        let _: Box<dyn AudioBackend> = Box::new(CpalBackend::new());
    }

    /// Verify the input pipeline does the right thing without touching
    /// cpal: feeds device-rate mono samples through `process_input` and
    /// checks the sink receives target-rate samples + a meter reading.
    #[test]
    fn process_input_resamples_and_meters() {
        #[derive(Default)]
        struct CaptureSink {
            samples: Arc<Mutex<Vec<f32>>>,
            last_meter: Arc<Mutex<f32>>,
        }
        impl InputSink for CaptureSink {
            fn write(&mut self, samples: &[f32], meter: f32) {
                self.samples.lock().unwrap().extend_from_slice(samples);
                *self.last_meter.lock().unwrap() = meter;
            }
        }

        let sink = CaptureSink::default();
        let captured = Arc::clone(&sink.samples);
        let metered = Arc::clone(&sink.last_meter);
        let mut sink: Box<dyn InputSink> = Box::new(sink);

        let mut resampler = Some(AntiAliasResampler::new(48_000, 8_000).unwrap());
        let mut mono = Vec::new();
        let mut resampled = Vec::new();

        // 50 ms of 1 kHz tone @ 48 kHz, mono.
        let n = 48_000 / 20;
        let device: Vec<f32> = (0..n)
            .map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / 48_000.0).sin() * 0.5)
            .collect();

        process_input(
            &device,
            1,
            1,
            resampler.as_mut(),
            &mut mono,
            &mut resampled,
            sink.as_mut(),
        );

        let cap = captured.lock().unwrap();
        assert!(!cap.is_empty(), "sink received nothing");
        assert!(cap.iter().all(|v| v.is_finite()));
        let m = *metered.lock().unwrap();
        assert!(m > 0.4 && m <= 1.0, "meter out of range: {m}");
    }

    /// Verify the output pipeline fills the cpal-side buffer to capacity
    /// even if the source provides fewer samples than requested.
    #[test]
    fn fill_output_pads_with_silence_when_starved() {
        struct EmptySrc;
        impl OutputSource for EmptySrc {
            fn read(&mut self, _out: &mut [f32]) -> usize {
                0
            }
        }
        let mut src: Box<dyn OutputSource> = Box::new(EmptySrc);
        let mut src_buf = Vec::new();
        let mut up_buf = Vec::new();
        let mut residual = std::collections::VecDeque::new();
        let mut out = vec![1.0_f32; 480]; // pre-fill to verify zeroing

        fill_output(
            &mut out,
            1,
            1,
            48_000,
            8_000,
            None,
            &mut src_buf,
            &mut up_buf,
            &mut residual,
            src.as_mut(),
        );
        assert!(out.iter().all(|v| *v == 0.0), "expected all-zero output");
    }

    /// Regression for iax-0b3f ("robotic" RX audio): network voice arrives in
    /// 160-sample (20 ms) chunks while `fill_output` requests with headroom.
    /// The unfilled padding must NEVER be fed into the stream as fabricated
    /// silence — with plenty of real audio queued (merely chunk-limited per
    /// read), the output across several callbacks must be (almost) free of
    /// exact-zero samples.
    #[test]
    fn fill_output_does_not_inject_padding_between_chunks() {
        /// Yields at most 160 samples of constant 0.5 per read call.
        struct ChunkedSrc {
            left: usize,
        }
        impl OutputSource for ChunkedSrc {
            fn read(&mut self, out: &mut [f32]) -> usize {
                let n = out.len().min(160).min(self.left);
                for v in out.iter_mut().take(n) {
                    *v = 0.5;
                }
                self.left -= n;
                n
            }
        }
        let mut src: Box<dyn OutputSource> = Box::new(ChunkedSrc { left: 8_000 });
        let mut resampler = AntiAliasResampler::new(8_000, 48_000).expect("resampler");
        let mut src_buf = Vec::new();
        let mut up_buf = Vec::new();
        let mut residual = std::collections::VecDeque::new();

        // Several device callbacks' worth of output (mono, 8 kHz → 48 kHz).
        let mut zeros = 0_usize;
        let mut total = 0_usize;
        for _ in 0..6 {
            let mut out = vec![9.9_f32; 512];
            fill_output(
                &mut out,
                1,
                1,
                48_000,
                8_000,
                Some(&mut resampler),
                &mut src_buf,
                &mut up_buf,
                &mut residual,
                src.as_mut(),
            );
            zeros += out.iter().filter(|s| **s == 0.0).count();
            total += out.len();
        }
        // Allow the resampler warm-up transient (measured 18 samples with the
        // former FastFixedIn-based Resampler1; measured 0 with the
        // AntiAliasResampler/SincFixedIn swap of iax-6945 — the sinc's
        // internal delay line only emits its first real output once the
        // 8 kHz source's first chunk has propagated through, and the source
        // here is non-zero from sample 0). Anything beyond this bound means
        // padding zeros leaked into the stream between real chunks (the
        // pre-fix behaviour measures in the thousands: ~2/3 of every
        // over-sized request was fabricated).
        assert!(
            zeros <= 64,
            "fabricated silence in the stream: {zeros} of {total} samples are exactly 0.0"
        );
    }

    /// Source that always emits a constant sample. Lets us verify the
    /// output mono → multi-channel expansion when the device wants stereo.
    #[test]
    fn fill_output_expands_mono_to_stereo() {
        struct ConstSrc(f32);
        impl OutputSource for ConstSrc {
            fn read(&mut self, out: &mut [f32]) -> usize {
                for v in out.iter_mut() {
                    *v = self.0;
                }
                out.len()
            }
        }
        let mut src: Box<dyn OutputSource> = Box::new(ConstSrc(0.25));
        let mut src_buf = Vec::new();
        let mut up_buf = Vec::new();
        let mut residual = std::collections::VecDeque::new();
        let mut out = vec![0.0_f32; 32 * 2]; // 32 stereo frames

        fill_output(
            &mut out,
            2,
            1,
            8_000, // no resampling — keep the math obvious
            8_000,
            None,
            &mut src_buf,
            &mut up_buf,
            &mut residual,
            src.as_mut(),
        );

        // Each pair should hold the same mono sample.
        for pair in out.chunks_exact(2) {
            assert_eq!(pair[0], pair[1]);
            assert!((pair[0] - 0.25).abs() < 1e-6);
        }
    }
    // --- iax-8d21: no wait on a hot-unplugged device may be unbounded ---
    //
    // The bug these guard: every wait between the caller and the
    // stream-owning thread used `recv()`/`join()` with no timeout. When a USB
    // device (Rob's UCI150) was unplugged and replugged, the thread wedged
    // inside a CoreAudio call on the dead device and never answered — so
    // dialling never returned, and neither did Quit, because `Station`'s Drop
    // chain runs through the same waits. The application could only be killed.

    mod wedge_bounds {
        use super::super::{await_reply, join_before};
        use crate::error::AudioError;
        use std::sync::mpsc::channel;
        use std::time::{Duration, Instant};

        const SHORT: Duration = Duration::from_millis(150);

        #[test]
        fn await_reply_returns_the_threads_answer() {
            let (tx, rx) = channel::<Result<(), AudioError>>();
            tx.send(Ok(())).unwrap();
            assert!(await_reply(&rx, SHORT).is_ok());

            let (tx, rx) = channel::<Result<(), AudioError>>();
            tx.send(Err(AudioError::StreamControl("nope".into())))
                .unwrap();
            assert!(
                await_reply(&rx, SHORT).is_err(),
                "a real failure still surfaces"
            );
        }

        #[test]
        fn await_reply_gives_up_on_a_thread_that_never_answers() {
            // Hold the sender so the channel stays OPEN: this is the wedged
            // device, not a closed channel. Without a timeout this recv would
            // block forever — which is the hang.
            let (_held, rx) = channel::<Result<(), AudioError>>();
            let started = Instant::now();
            let err = await_reply(&rx, SHORT).expect_err("a silent thread must not block forever");
            assert!(
                started.elapsed() < SHORT * 8,
                "await_reply must return near its timeout, not hang (took {:?})",
                started.elapsed()
            );
            let msg = err.to_string();
            assert!(
                msg.contains("disabled"),
                "the operator needs to see a disabled device, not a generic failure: {msg}"
            );
        }

        #[test]
        fn await_reply_reports_a_closed_channel_distinctly() {
            let (tx, rx) = channel::<Result<(), AudioError>>();
            drop(tx);
            let msg = await_reply(&rx, SHORT)
                .expect_err("closed is still an error")
                .to_string();
            assert!(msg.contains("closed"), "got {msg}");
        }

        #[test]
        fn join_before_joins_a_thread_that_signalled_completion() {
            let (done_tx, done_rx) = channel::<()>();
            let h = std::thread::spawn(move || {
                let _ = done_tx.send(());
            });
            assert!(
                join_before(h, &done_rx, SHORT),
                "a healthy thread is joined normally"
            );
        }

        #[test]
        fn join_before_detaches_a_wedged_thread_instead_of_hanging() {
            // A thread that never signals — the CoreAudio wedge. Joining it
            // is what froze Quit; `join_before` must give up and detach.
            let (_held, done_rx) = channel::<()>();
            let h = std::thread::spawn(|| std::thread::sleep(Duration::from_secs(30)));
            let started = Instant::now();
            assert!(
                !join_before(h, &done_rx, SHORT),
                "a wedged thread must be reported as detached, not joined"
            );
            assert!(
                started.elapsed() < SHORT * 8,
                "join_before must return near its timeout, not wait out the thread (took {:?})",
                started.elapsed()
            );
        }
    }
}
