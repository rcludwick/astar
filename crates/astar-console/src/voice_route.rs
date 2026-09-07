// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The audio lane a digital-voice session (M17, D-Star, System Fusion) is
//! handed, opened on the station's ONE router.
//!
//! A session used to build its own `AudioRouter` over its own backend, on
//! its own thread, and so had to mirror meters, carry preferences, decide
//! when the microphone opened and key the gate — three sessions, three
//! answers, and every gap between them was a bug. Now the session gets the
//! channel ends and nothing else; everything about devices, meters, DSP
//! and keying lives here and in `ConsoleSession`, once.
//!
//! Capture policy: the lane opens at connect if a device resolves and opens
//! (gate closed), so the continuous input meter and VOX work from the first
//! second, as on IAX2 and M17. If it cannot, the route is receive-only for
//! now and every key-down retries — a machine with no usable microphone
//! must still receive.
//!
//! # Rate bridging (the 16 kHz station)
//!
//! The station's ONE bus runs at the pipeline rate the codec policy pinned
//! (iax-4348): 8 kHz, or 16 kHz for a `prefer_slin16` station — which astar
//! and astar-server both are. The digital-voice codecs are not: M17's
//! Codec2, D-Star's and System Fusion's AMBE all speak 160-sample 8 kHz
//! frames and nothing else. Mixing the two on one bus is silent corruption
//! (`Manager::adopt` refuses it outright), and `AudioRouter::ensure_output`
//! /`ensure_mic` reuse an already-open lane whatever config is passed, so
//! the route cannot simply ask for 8 kHz and hope.
//!
//! So the lanes are opened at the BUS rate and this module bridges: on an
//! 8 kHz station the session's channel ends are the bus's, passed straight
//! through at zero cost (today's behaviour, byte for byte); on a 16 kHz
//! station two small threads convert 20 ms frame for 20 ms frame — 160
//! samples in ↔ 320 samples out, exactly, every frame — using the same
//! fixed-framing recipe as the IAX2 codec edge (`Resampler1::with_chunk` at
//! one frame per pass, a FIFO that pops exactly one output frame per input
//! frame, and a Butterworth anti-alias cascade ahead of the downsample).
//! The session sees the same [`CallAudio`] contract either way, and
//! `preroll_lead` needs no scaling because it counts FRAMES.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use astar_audio::mixer::MixCallId;
use astar_audio::{
    AudioError, AudioRouter, Biquad, CallAudio, MicId, OutputId, Resampler1, StreamConfig,
    StreamHandle,
};

/// The rate every digital-voice codec here speaks: Codec2 (M17) and AMBE
/// (D-Star, System Fusion) are 8 kHz, 160 samples per 20 ms frame, fixed.
pub(crate) const SESSION_RATE: u32 = 8_000;

/// How often a bridge thread wakes to notice [`Bridge::join`]'s stop flag.
/// One frame period: the cost is a wakeup per 20 ms per direction (nothing
/// beside the audio itself) and it bounds the join a disconnect runs under
/// the session lock.
const POLL: Duration = Duration::from_millis(20);

/// Samples in one 20 ms frame at `rate`.
const fn frame_len(rate: u32) -> usize {
    (rate / 50) as usize
}

/// Pop exactly `frame` samples from `fifo`, left-padding with silence while
/// the resampler is still warming up (start-up only — steady state is one
/// frame in, one frame out). Mirrors the codec edge's own `pop_frame`.
fn pop_frame(fifo: &mut Vec<i16>, frame: usize) -> Vec<i16> {
    if fifo.len() < frame {
        let missing = frame - fifo.len();
        let mut out = vec![0i16; missing];
        out.append(fifo);
        return out;
    }
    fifo.drain(..frame).collect()
}

/// Anti-alias low-pass for a DOWNSAMPLE: a 4th-order Butterworth (two
/// cascaded biquads with the Butterworth Q pair) cornered at 0.45 × the
/// target rate — 0.9 × the target Nyquist, so the voice band passes intact
/// while everything that would fold is rolled off first. [`Resampler1`] is
/// filterless linear interpolation, so without this a 16 kHz bus folds its
/// 4–8 kHz energy straight into the 8 kHz session band.
#[allow(clippy::cast_precision_loss)]
fn antialias_lowpass(from: u32, to: u32) -> Vec<Biquad> {
    let cutoff = 0.45 * to as f32;
    [0.541_196_1_f32, 1.306_563]
        .iter()
        .map(|&q| Biquad::lowpass(from, cutoff, q))
        .collect()
}

/// One direction of the rate bridge: exactly one 20 ms frame in, exactly
/// one 20 ms frame out, no drift.
struct RateBridge {
    /// Anti-alias cascade run at `from` before a downsample; empty upward.
    aa: Vec<Biquad>,
    rs: Resampler1,
    /// Resampled-but-not-yet-framed output. The interpolator holds back a
    /// few warm-up samples, so the first pass runs short; this lets every
    /// call emit a full frame instead of propagating a short one.
    fifo: Vec<i16>,
    out_frame: usize,
}

impl RateBridge {
    fn new(from: u32, to: u32) -> Result<Self, AudioError> {
        Ok(Self {
            aa: if to < from {
                antialias_lowpass(from, to)
            } else {
                Vec::new()
            },
            // Chunk = one 20 ms input frame: with the integer 8↔16 kHz
            // ratio, one frame in yields exactly one frame out.
            rs: Resampler1::with_chunk(from, to, frame_len(from))?,
            fifo: Vec::new(),
            out_frame: frame_len(to),
        })
    }

    /// Convert one frame. `None` only on an internal resampler failure, in
    /// which case the frame is dropped rather than the route poisoned.
    #[allow(clippy::cast_possible_truncation)]
    fn convert(&mut self, pcm: &[i16]) -> Option<Vec<i16>> {
        let mut f: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32768.0).collect();
        for bq in &mut self.aa {
            bq.process(&mut f);
        }
        self.rs.push(&f).ok()?;
        let out = self.rs.drain_all();
        self.fifo
            .extend(out.iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
        Some(pop_frame(&mut self.fifo, self.out_frame))
    }
}

/// The pair of converter threads a cross-rate route runs, and the flag that
/// stops them.
struct Bridge {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Bridge {
    /// Stop both threads and wait for them. Bounded by [`POLL`]: the flag is
    /// set for both before either is joined, and each wakes to check it at
    /// least that often even with no traffic at all.
    fn join(self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads {
            if t.join().is_err() {
                tracing::warn!("voice route: a rate-bridge thread panicked");
            }
        }
    }
}

/// Convert every frame from `rx` and forward it to `tx` until either end
/// disconnects or `stop` is set.
fn pump(rx: &Receiver<Vec<i16>>, tx: &Sender<Vec<i16>>, mut bridge: RateBridge, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(POLL) {
            Ok(frame) => {
                if let Some(out) = bridge.convert(&frame)
                    && tx.send(out).is_err()
                {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Wrap the bus-rate [`CallAudio`] in a pair of converters and hand back the
/// 8 kHz ends the session expects.
fn spawn_bridge(bus: CallAudio, bus_rate: u32) -> Result<(CallAudio, Bridge), AudioError> {
    let preroll_lead = Arc::clone(&bus.preroll_lead);
    let CallAudio {
        tx_frames: bus_tx,
        rx_frames: bus_rx,
        ..
    } = bus;
    let stop = Arc::new(AtomicBool::new(false));

    // RX: the session decodes 8 kHz frames and sends them here; they reach
    // the bus upsampled.
    let (sess_rx_tx, sess_rx_rx) = channel::<Vec<i16>>();
    let up = RateBridge::new(SESSION_RATE, bus_rate)?;
    let stop_up = Arc::clone(&stop);
    let rx_thread = std::thread::Builder::new()
        .name("astar-voice-bridge-rx".into())
        .spawn(move || pump(&sess_rx_rx, &bus_rx, up, &stop_up))
        .map_err(|e| AudioError::BuildStream(format!("voice bridge (rx) thread: {e}")))?;

    // TX: the mic lane captures at the bus rate; the session gets 8 kHz.
    let (sess_tx_tx, sess_tx_rx) = channel::<Vec<i16>>();
    let down = RateBridge::new(bus_rate, SESSION_RATE)?;
    let stop_down = Arc::clone(&stop);
    let tx_thread = std::thread::Builder::new()
        .name("astar-voice-bridge-tx".into())
        .spawn(move || pump(&bus_tx, &sess_tx_tx, down, &stop_down))
        .map_err(|e| AudioError::BuildStream(format!("voice bridge (tx) thread: {e}")))?;

    Ok((
        CallAudio {
            tx_frames: sess_tx_rx,
            rx_frames: sess_rx_tx,
            // The SAME cell the mic lane writes: pre-roll counts frames, and
            // a frame is a frame at either rate.
            preroll_lead,
        },
        Bridge {
            stop,
            threads: vec![rx_thread, tx_thread],
        },
    ))
}

/// One digital-voice session's lanes on the station router: an output bus
/// slot, and — when a capture device resolved — a mic lane whose gate is the
/// session's PTT.
pub(crate) struct VoiceRoute {
    out: OutputId,
    mix_id: MixCallId,
    /// The resolved capture device, `None` on a receive-only machine.
    mic: Option<MicId>,
    /// Parked until the lane opens, then handed to the router.
    mic_tx: Option<Sender<Vec<i16>>>,
    preroll_lead: Arc<AtomicU32>,
    config: StreamConfig,
    mic_open: bool,
    /// The rate converters, when the bus does not run at [`SESSION_RATE`].
    /// `None` on an 8 kHz station — the session holds the bus's own channel
    /// ends and nothing sits in between.
    bridge: Option<Bridge>,
}

impl VoiceRoute {
    /// Open the bus on `router` (and, if `mic` is `Some`, try the lane) at
    /// `config`'s rate — the STATION's pipeline rate, not the session's — and
    /// return the route plus the session's 8 kHz channel ends, rate-bridged
    /// when the two differ (see the module docs).
    ///
    /// # Errors
    /// Only the output bus is fatal; a capture lane that fails to open is
    /// logged and retried on key-down. [`AudioError::BuildStream`] if a
    /// bridge thread cannot be spawned on a cross-rate station.
    pub(crate) fn open(
        router: &mut AudioRouter,
        mic: Option<MicId>,
        out: OutputId,
        config: StreamConfig,
    ) -> Result<(VoiceRoute, CallAudio), astar_audio::AudioError> {
        let (bus_audio, mic_tx, mix_id) = router.open_monitor_call(&out, config)?;
        let (audio, bridge) = if config.sample_rate == SESSION_RATE {
            (bus_audio, None)
        } else {
            tracing::info!(
                bus_hz = config.sample_rate,
                session_hz = SESSION_RATE,
                "voice route: rate-bridging the digital session onto the station bus"
            );
            let (a, b) = spawn_bridge(bus_audio, config.sample_rate)?;
            (a, Some(b))
        };
        let mut route = VoiceRoute {
            out,
            mix_id,
            mic,
            mic_tx: Some(mic_tx),
            preroll_lead: Arc::clone(&audio.preroll_lead),
            config,
            mic_open: false,
            bridge,
        };
        // Eager, but not fatal.
        let _ = route.ensure_mic(router);
        Ok((route, audio))
    }

    /// `true` when this route converts between the bus rate and
    /// [`SESSION_RATE`] rather than passing the bus's own channel ends
    /// through. Diagnostic / test seam.
    #[cfg(test)]
    pub(crate) fn bridged(&self) -> bool {
        self.bridge.is_some()
    }

    pub(crate) fn out(&self) -> &OutputId {
        &self.out
    }

    /// The mic id whenever a device resolved — open or not — so meters and
    /// preferences address the lane the moment it exists.
    pub(crate) fn mic(&self) -> Option<&MicId> {
        self.mic.as_ref()
    }

    pub(crate) fn tx_capable(&self) -> bool {
        self.mic.is_some()
    }

    /// Open the capture lane if it isn't. `false` = this route cannot
    /// transmit right now (no device, or the open failed).
    pub(crate) fn ensure_mic(&mut self, router: &mut AudioRouter) -> bool {
        if self.mic_open {
            return true;
        }
        let Some(id) = self.mic.as_ref() else {
            return false;
        };
        let Some(tx) = self.mic_tx.take() else {
            return false;
        };
        match router.open_mic_lane(id, tx.clone(), Arc::clone(&self.preroll_lead), self.config) {
            Ok(()) => {
                self.mic_open = true;
                true
            }
            Err(e) => {
                // Park the sender again: the device may come back, or
                // permission may be granted, before the next key-down.
                self.mic_tx = Some(tx);
                tracing::warn!(
                    error = ?e,
                    "voice route: capture device would not open — receive only for now"
                );
                false
            }
        }
    }

    /// Key or unkey. `false` = refused (see [`Self::ensure_mic`]); the
    /// caller must not transmit.
    pub(crate) fn key(&mut self, router: &mut AudioRouter, on: bool) -> bool {
        if on && !self.ensure_mic(router) {
            return false;
        }
        if let Some(id) = self.mic.as_ref() {
            router.set_gate(id, on);
        }
        true
    }

    /// Unbind and close both lanes. Returns the stream handles for the
    /// caller to drop outside any lock.
    pub(crate) fn release(self, router: &mut AudioRouter) -> Vec<Box<dyn StreamHandle>> {
        let mut handles = Vec::with_capacity(2);
        if let Some(id) = self.mic.as_ref() {
            router.set_gate(id, false);
            router.unbind_mic(id);
            if let Some(handle) = router.close_mic(id) {
                handles.push(handle);
            } else {
                // `unbind_mic` above cleared the destination, so the router
                // has no reason to refuse — unless something re-bound the
                // lane under us. Say so rather than leak a capture device
                // silently (see the output-bus branch below).
                tracing::warn!(
                    mic = id.as_str(),
                    "voice route: capture lane still bound at release — stream left open"
                );
            }
        }
        router.remove_from_bus(&self.out, self.mix_id);
        if let Some(handle) = router.close_output(&self.out) {
            handles.push(handle);
        } else {
            // The bus still carries a lane, so the stream stays open and its
            // handle stays with the router. The exclusion guards should make
            // that impossible for a voice route; say so rather than leak an
            // output device silently if they ever stop holding.
            tracing::warn!(
                bus = self.out.as_str(),
                "voice route: output bus still in use at release — stream left open"
            );
        }
        // Both lanes are closed, so both bridge channels are already
        // disconnected on the bus side; the stop flag covers the session side
        // (whose `CallAudio` the caller may still be holding) and bounds this
        // to one frame period.
        if let Some(bridge) = self.bridge {
            bridge.join();
        }
        handles
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_audio::NullBackend;
    use std::sync::atomic::AtomicU32;

    /// `n` samples of a continuous `freq` sine at `rate`, as i16.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn sine(rate: u32, freq: f32, n: usize) -> Vec<i16> {
        (0..n)
            .map(|i| {
                let t = i as f32 / rate as f32;
                (0.6 * (std::f32::consts::TAU * freq * t).sin() * 32767.0) as i16
            })
            .collect()
    }

    /// Sign changes — a rate-agnostic pitch measure: a tone that survives a
    /// conversion at the right pitch crosses zero the same number of times
    /// per unit of TIME whatever the sample rate.
    fn zero_crossings(pcm: &[i16]) -> usize {
        pcm.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count()
    }

    fn bus_pair() -> (
        CallAudio,
        Receiver<Vec<i16>>, // what the bridge pushes at the bus rate
        Sender<Vec<i16>>,   // what the mic lane would push at the bus rate
    ) {
        let (to_bus, from_bridge) = channel::<Vec<i16>>();
        let (to_bridge, from_mic) = channel::<Vec<i16>>();
        (
            CallAudio {
                tx_frames: from_mic,
                rx_frames: to_bus,
                preroll_lead: Arc::new(AtomicU32::new(0)),
            },
            from_bridge,
            to_bridge,
        )
    }

    /// The heart of the 16 kHz station: 160 samples in ↔ 320 samples out,
    /// EXACTLY, every frame, in both directions — and a 1 kHz tone that comes
    /// out at 1 kHz, not at half or double. Anything less is either a
    /// stuttering bus (short/empty frames on a fixed 20 ms cadence) or
    /// chipmunk audio.
    #[test]
    fn a_16k_bridge_frames_exactly_and_keeps_pitch_both_ways() {
        const FRAMES: usize = 30;
        const TONE_HZ: f32 = 1_000.0;
        /// Frames skipped before measuring: the resampler's warm-up
        /// left-pads the first frame and the anti-alias biquads settle.
        const WARMUP: usize = 2;

        let (bus, from_bridge, to_bridge) = bus_pair();
        let (sess, bridge) = spawn_bridge(bus, 16_000).expect("bridge spawns");

        // RX: the session decodes 8 kHz, the bus must receive 16 kHz.
        let up_in = sine(SESSION_RATE, TONE_HZ, 160 * FRAMES);
        for f in up_in.chunks(160) {
            sess.rx_frames.send(f.to_vec()).expect("session -> bridge");
        }
        let mut up_out = Vec::new();
        for i in 0..FRAMES {
            let f = from_bridge
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|e| panic!("bus frame {i}: {e}"));
            assert_eq!(f.len(), 320, "bus frame {i} must be one 20 ms 16 kHz frame");
            up_out.extend_from_slice(&f);
        }
        let want_up = zero_crossings(&up_in[160 * WARMUP..]);
        let got_up = zero_crossings(&up_out[320 * WARMUP..]);
        assert!(
            got_up.abs_diff(want_up) * 50 < want_up,
            "upsampled tone changed pitch: {got_up} zero crossings vs {want_up}"
        );

        // TX: the mic lane captures 16 kHz, the session must receive 8 kHz.
        let down_in = sine(16_000, TONE_HZ, 320 * FRAMES);
        for f in down_in.chunks(320) {
            to_bridge.send(f.to_vec()).expect("mic -> bridge");
        }
        let mut down_out = Vec::new();
        for i in 0..FRAMES {
            let f = sess
                .tx_frames
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|e| panic!("session frame {i}: {e}"));
            assert_eq!(
                f.len(),
                160,
                "session frame {i} must be one 20 ms 8 kHz frame"
            );
            down_out.extend_from_slice(&f);
        }
        let want_dn = zero_crossings(&down_in[320 * WARMUP..]);
        let got_dn = zero_crossings(&down_out[160 * WARMUP..]);
        assert!(
            got_dn.abs_diff(want_dn) * 50 < want_dn,
            "downsampled tone changed pitch: {got_dn} zero crossings vs {want_dn}"
        );

        bridge.join();
    }

    /// The bridge threads exit on their own when the bus side goes away —
    /// `join` must not depend on the session dropping its `CallAudio` first
    /// (a disconnect runs it under the session lock).
    #[test]
    fn the_bridge_joins_even_while_the_session_still_holds_its_ends() {
        let (bus, from_bridge, to_bridge) = bus_pair();
        let (sess, bridge) = spawn_bridge(bus, 16_000).expect("bridge spawns");
        drop(from_bridge);
        drop(to_bridge);
        bridge.join();
        drop(sess);
    }

    fn null_router() -> (AudioRouter, MicId, OutputId) {
        let backend = NullBackend::new();
        let mic = MicId::new("in:null");
        let out = OutputId::new("out:null");
        (AudioRouter::new(Box::new(backend)), mic, out)
    }

    /// An 8 kHz station is byte-identical to before the bridge existed: the
    /// session holds the bus's OWN channel ends and no thread sits between.
    #[test]
    fn an_8k_station_passes_the_route_through() {
        let (mut router, mic, out) = null_router();
        let (route, _audio) =
            VoiceRoute::open(&mut router, Some(mic), out, StreamConfig::default())
                .expect("route opens");
        assert!(!route.bridged(), "an 8 kHz bus needs no converter");
        drop(route.release(&mut router));
    }

    /// A 16 kHz station opens its lanes at 16 kHz and bridges.
    #[test]
    fn a_16k_station_bridges_the_route() {
        let (mut router, mic, out) = null_router();
        let cfg = StreamConfig {
            sample_rate: 16_000,
            ..StreamConfig::default()
        };
        let (route, _audio) =
            VoiceRoute::open(&mut router, Some(mic), out, cfg).expect("route opens");
        assert!(route.bridged(), "a 16 kHz bus must be bridged to 8 kHz");
        drop(route.release(&mut router));
    }
}
