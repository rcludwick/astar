// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Audio device router: owns cpal streams and hands each call its channel ends.
//!
//! Phase 1 (iax-42e9): a trivial 1-mic / 1-output form. The call runtime is
//! already channel-based — it consumes `tx_frames` (20 ms PCM TX frames)
//! and emits to `rx_frames` (PCM RX frames). Codec transcode happens at the
//! network edge (iax-31f7), not in the audio layer. The router owns the streams
//! so `Client::dial` no longer does. Phase 2 grows this to N lanes/buses.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use crate::meter::peak;
use crate::mixer::Mixer;
use crate::spectrum::SpectrumAnalyzer;
use crate::{
    AudioBackend, AudioError, Compressor, CompressorParams, DeviceInfo, Direction, InputSink,
    MicProfile, NoiseReducer, OutputSource, StreamConfig, StreamHandle,
};

/// Identifies a capture device within the router (mics are 1:1, never shared).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MicId(String);
impl MicId {
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identifies a playback device within the router (outputs may be shared/summed).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OutputId(String);
impl OutputId {
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The two channel ends a call runtime needs. The router keeps the other ends.
pub struct CallAudio {
    /// 20 ms PCM TX frames the run-loop encodes and pumps onto the wire.
    pub tx_frames: Receiver<Vec<i16>>,
    /// Decoded PCM RX frames the run-loop forwards from `VoiceReceived`.
    pub rx_frames: Sender<Vec<i16>>,
    /// VOX pre-roll lead signal (iax-2733): the mic lane stores the number of
    /// look-back frames it just flushed AHEAD of the live stream on each
    /// gate-up edge; the run-loop reads-and-clears it to keep its media-clock
    /// ladder from mistaking the deliberate lead for >80 ms drift. Always `0`
    /// when pre-roll is disabled (`preroll_ms == 0`), so the ladder path is
    /// byte-identical to today in that case. Shared with the lane (audio
    /// thread) and the run-loop (call thread).
    pub preroll_lead: Arc<AtomicU32>,
}

/// One open capture device: its stream handle plus the live control cells the
/// lane runs on (swappable destination, PTT gate, TX peak). Phase 2 static
/// wiring; Phase 3's `route()` swaps `dest`.
struct MicSlot {
    #[allow(dead_code)] // kept alive so the cpal input stream stays open
    handle: Box<dyn StreamHandle>,
    dest: MicDest,
    gate: Arc<AtomicBool>,
    /// Capture-gain cell (f32 bits, default 1.0) shared with the lane.
    gain: Arc<AtomicU32>,
    /// Noise-reduction toggle shared with the lane.
    denoise: Arc<AtomicBool>,
    /// Compressor toggle shared with the lane.
    compress: Arc<AtomicBool>,
    /// Compressor strength cell (f32 bits, 0.0..=1.0) shared with the lane.
    compress_level: Arc<AtomicU32>,
    /// TX trim cell (f32 bits, 0.0..=2.0, default 1.0 = unity) shared with the
    /// lane: the always-on final gain stage after the compressor (iax-750a).
    tx_trim: Arc<AtomicU32>,
    /// Calibrated per-mic profile cell; the lane rebuilds its NR on `profile_gen` bump.
    profile: Arc<Mutex<Option<MicProfile>>>,
    /// Generation counter the lane watches to rebuild NR from `profile` on the audio thread.
    profile_gen: Arc<AtomicU32>,
    /// Shared TX peak cell (f32 bits) for per-lane metering.
    tx_peak: Arc<AtomicU32>,
    /// Shared live TX spectrum analyzer (iax-2b09): the lane feeds it the
    /// post-DSP, pre-encode TX PCM so a harness can draw the outgoing-audio
    /// spectrum during a call.
    tx_spectrum: Arc<Mutex<SpectrumAnalyzer>>,
    /// Shared mic INPUT peak cell (f32 bits): post-gain, pre-NoiseReducer, and
    /// metered CONTINUOUSLY — even while unkeyed — so a consumer's VOX can key
    /// from silence (iax-5c30).
    mic_input_peak: Arc<AtomicU32>,
    /// VOX pre-roll length in ms (iax-2733): how much look-back audio the lane
    /// flushes ahead of the live stream on key-up. `0` = disabled (the lane
    /// keeps today's cheap unkeyed early-return; no ring, no unkeyed DSP).
    /// Clamped to `0..=MAX_PREROLL_MS` by the control side.
    preroll_ms: Arc<AtomicU32>,
    /// Cumulative cpal capture overruns on this mic's input stream (iax-9e55):
    /// dropped input buffers (holes in the captured PCM), the lead suspect for
    /// choppy TX. Bumped on the capture thread by `CaptureGapWatch`; read via
    /// [`AudioRouter::mic_capture_overruns`]. A plain `u64` health counter.
    overruns: Arc<AtomicU64>,
    /// Active TX announcement injected ahead of the mic (iax-e30d).
    inject: InjectCell,
}

/// One open playback device: its stream handle plus the shared `Mixer` the
/// router adds/removes call RX channels to, and the bus RX peak cell.
struct OutSlot {
    #[allow(dead_code)] // kept alive so the cpal output stream stays open
    handle: Box<dyn StreamHandle>,
    mixer: Arc<Mutex<Mixer>>,
    /// Shared output-gain cell (f32 bits, default 1.0) for this bus.
    rx_gain: Arc<AtomicU32>,
    /// Shared RX peak cell (f32 bits) for per-bus metering.
    rx_peak: Arc<AtomicU32>,
    /// Shared live RX spectrum analyzer (iax-2b09): the bus feeds it the
    /// post-mix decoded RX PCM so a harness can draw the received-audio
    /// spectrum during a call.
    rx_spectrum: Arc<Mutex<SpectrumAnalyzer>>,
    /// RX/output compression toggle shared with the bus (iax-a4e7 PHASE 1).
    rx_compress: Arc<AtomicBool>,
    /// RX/output compression strength cell (f32 bits, 0.0..=1.0) shared with
    /// the bus.
    rx_compress_level: Arc<AtomicU32>,
}

/// Owns the audio backend + the open streams. Phase 2: N mic lanes / N output
/// buses. A mic opens at most once (mics are 1:1, never shared); an output
/// opens at most once and is summed across the calls routed to it.
pub struct AudioRouter {
    backend: Box<dyn AudioBackend>,
    mics: HashMap<MicId, MicSlot>,
    outputs: HashMap<OutputId, OutSlot>,
}

impl AudioRouter {
    /// Construct a router over `backend`. The router owns the open streams so
    /// `Client`/`Manager` no longer do.
    #[must_use]
    pub fn new(backend: Box<dyn AudioBackend>) -> Self {
        Self {
            backend,
            mics: HashMap::new(),
            outputs: HashMap::new(),
        }
    }

    /// Enumerate the backend's devices (pass-through for caller-side device
    /// resolution in `Client::dial`).
    ///
    /// # Errors
    /// Propagates [`AudioError`] from the backend.
    pub fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        self.backend.devices()
    }

    /// The backend's default input device, if any.
    #[must_use]
    pub fn default_input(&self) -> Option<DeviceInfo> {
        self.backend.default_input()
    }

    /// The backend's default output device, if any.
    #[must_use]
    pub fn default_output(&self) -> Option<DeviceInfo> {
        self.backend.default_output()
    }

    /// Resolve a device by its `DeviceInfo.id` string against the enumerated list.
    fn resolve(&self, id: &str, dir: Direction) -> Result<DeviceInfo, AudioError> {
        self.backend
            .devices()?
            .into_iter()
            .find(|d| {
                d.id.as_str() == id && (d.direction == dir || d.direction == Direction::Duplex)
            })
            .ok_or_else(|| AudioError::DeviceNotFound(id.to_string()))
    }

    /// Open (or reuse) the mic lane + output bus and return the call's channel
    /// ends. A mic opens at most once; an output opens at most once and its
    /// `Mixer` gains one lane per call routed to it. Static wiring (Phase 2):
    /// the returned TX channel is wired straight into the mic lane's
    /// destination with a throwaway waker — Phase 3's `Manager` supplies the
    /// per-call run-loop waker and swaps the dest on `route()`.
    ///
    /// # Errors
    /// Propagates [`AudioError`] from device resolution / stream open.
    pub fn open_call(
        &mut self,
        mic: &MicId,
        out: &OutputId,
        config: StreamConfig,
    ) -> Result<CallAudio, AudioError> {
        // 1. Open or reuse the output bus, then add this call's RX lane.
        let (spk_tx, spk_rx) = channel::<Vec<i16>>();
        self.ensure_output(out, config)?;
        let out_slot = self
            .outputs
            .get(out)
            .expect("output bus just ensured present");
        out_slot
            .mixer
            .lock()
            .expect("mixer mutex poisoned")
            .add_call(spk_rx);

        // 2. Open or reuse the mic lane, then wire this call's TX destination.
        let (mic_tx, mic_rx) = channel::<Vec<i16>>();
        self.ensure_mic(mic, config)?;
        let mic_slot = self.mics.get(mic).expect("mic lane just ensured present");
        let waker = throwaway_waker();
        // VOX pre-roll lead cell (iax-2733): owned by the call, handed to the
        // lane in the dest unit and back to the run-loop via CallAudio.
        let preroll_lead = Arc::new(AtomicU32::new(0));
        *mic_slot.dest.lock().expect("dest mutex poisoned") =
            Some((mic_tx, waker, Arc::clone(&preroll_lead)));
        // Static wiring: a freshly opened/routed mic is keyed.
        mic_slot.gate.store(true, Ordering::Relaxed);

        Ok(CallAudio {
            tx_frames: mic_rx,
            rx_frames: spk_tx,
            preroll_lead,
        })
    }

    /// Open (or join) the output bus for a monitor-only call: register the
    /// call's RX channel on the bus mixer, and create — but do NOT wire — the
    /// call's TX channel. Returns the call's [`CallAudio`] (the run-loop ends),
    /// the parked TX `Sender` (handed to [`AudioRouter::bind_mic`] on `route`),
    /// and the bus slot id (kept by the `Manager` for `set_output`/teardown).
    ///
    /// A freshly dialed/adopted call is monitor-only: its RX mixes to the
    /// speaker, but no mic feeds it until `route()`.
    ///
    /// # Errors
    /// Propagates [`AudioError`] from device resolution / stream open.
    pub fn open_monitor_call(
        &mut self,
        out: &OutputId,
        config: StreamConfig,
    ) -> Result<(CallAudio, Sender<Vec<i16>>, crate::mixer::MixCallId), AudioError> {
        // RX side: open/join the bus and add this call's lane.
        let (spk_tx, spk_rx) = channel::<Vec<i16>>();
        self.ensure_output(out, config)?;
        let mix_id = self
            .outputs
            .get(out)
            .expect("output bus just ensured present")
            .mixer
            .lock()
            .expect("mixer mutex poisoned")
            .add_call(spk_rx);

        // TX side: the channel the run-loop drains; the matching Sender is
        // parked (no mic) until `route()` binds it to a mic lane. The pre-roll
        // lead cell (iax-2733) is owned by the call and carried into the mic
        // lane's dest unit when `route()`/`bind_mic` binds the Sender.
        let (mic_tx, mic_rx) = channel::<Vec<i16>>();
        Ok((
            CallAudio {
                tx_frames: mic_rx,
                rx_frames: spk_tx,
                preroll_lead: Arc::new(AtomicU32::new(0)),
            },
            mic_tx,
            mix_id,
        ))
    }

    /// Point a mic lane's swappable destination at a call's TX `Sender` + that
    /// call's run-loop `Waker` (the Q1 unit). Opens the mic lane if needed. Does
    /// NOT key the gate — the `Manager` keys explicitly. Re-binding atomically
    /// replaces any previous `(Sender, Waker)` on this mic.
    ///
    /// # Errors
    /// Propagates [`AudioError`] from device resolution / stream open.
    pub fn bind_mic(
        &mut self,
        mic: &MicId,
        dest: MicDestUnit,
        config: StreamConfig,
    ) -> Result<(), AudioError> {
        self.ensure_mic(mic, config)?;
        let slot = self.mics.get(mic).expect("mic lane just ensured present");
        *slot.dest.lock().expect("dest mutex poisoned") = Some(dest);
        Ok(())
    }

    /// Open (or reuse) a mic lane and wire it to `tx` with a throwaway waker,
    /// leaving the gate CLOSED — the "upgrade a monitor call to a transmit
    /// call, later" half of [`AudioRouter::open_monitor_call`], for run loops
    /// that own their own poll cadence and therefore have no waker to hand in
    /// (see [`AudioRouter::open_call`], whose static wiring this mirrors
    /// exactly apart from the gate).
    ///
    /// Deliberately does NOT key the gate: a caller reaching for this is
    /// opening a capture device at the moment the operator asked to transmit,
    /// and the ONLY thing allowed to key a mic is that caller's own explicit
    /// [`AudioRouter::set_gate`]. `preroll_lead` is the cell the call already
    /// owns (from `open_monitor_call`'s [`CallAudio`]), carried into the lane
    /// so VOX pre-roll accounting stays attached to the same call.
    ///
    /// # Errors
    /// Propagates [`AudioError`] from device resolution / stream open — which
    /// is the point: a machine with no usable capture device fails HERE, when
    /// transmit is first requested, rather than at call setup (so a
    /// receive-only session on such a machine still works).
    pub fn open_mic_lane(
        &mut self,
        mic: &MicId,
        tx: Sender<Vec<i16>>,
        preroll_lead: Arc<AtomicU32>,
        config: StreamConfig,
    ) -> Result<(), AudioError> {
        self.ensure_mic(mic, config)?;
        let slot = self.mics.get(mic).expect("mic lane just ensured present");
        *slot.dest.lock().expect("dest mutex poisoned") =
            Some((tx, throwaway_waker(), preroll_lead));
        Ok(())
    }

    /// Clear a mic lane's destination (monitor-only / unroute). The lane stays
    /// open but emits nothing until re-bound. No-op if the mic isn't open.
    pub fn unbind_mic(&mut self, mic: &MicId) {
        if let Some(slot) = self.mics.get(mic) {
            *slot.dest.lock().expect("dest mutex poisoned") = None;
        }
    }

    /// Set a mic lane's PTT gate (`true` = keyed). No-op if the mic isn't open.
    pub fn set_gate(&self, mic: &MicId, keyed: bool) {
        if let Some(slot) = self.mics.get(mic) {
            slot.gate.store(keyed, Ordering::Relaxed);
        }
    }

    /// Queue an announcement on a mic lane's TX path (iax-e30d). The lane
    /// applies it on the capture callback per [`AnnouncePolicy`]. The caller is
    /// responsible for keying the lane (the announcement only transmits while
    /// the gate is open). Returns `None` if the mic isn't open. Replaces any
    /// announcement already playing on this lane.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)] // Arc<[i16]> is the intended public API
    pub fn play_into_mic(
        &self,
        mic: &MicId,
        pcm: Arc<[i16]>,
        policy: crate::AnnouncePolicy,
    ) -> Option<crate::AnnounceHandle> {
        let slot = self.mics.get(mic)?;
        let (handle, cells) = crate::AnnounceHandle::new();
        let pcm_f32 = pcm.iter().map(|&s| f32::from(s) / 32_768.0).collect();
        *slot.inject.lock().expect("inject mutex") = Some(Inject {
            pcm: pcm_f32,
            policy,
            cells,
        });
        Some(handle)
    }

    /// Play a finite announcement onto an output bus only (local monitor; no
    /// TX, no keying). Chunks the PCM into 20 ms frames at `sample_rate`,
    /// registers a finite mixer lane, and returns a handle that completes when
    /// the bus drains it. `None` if the bus isn't open.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)] // Arc<[i16]> is the intended public API
    pub fn play_into_bus(
        &self,
        out: &OutputId,
        pcm: Arc<[i16]>,
        sample_rate: u32,
    ) -> Option<crate::AnnounceHandle> {
        let slot = self.outputs.get(out)?;
        let (handle, cells) = crate::AnnounceHandle::new();
        let (tx, rx) = channel::<Vec<i16>>();
        for chunk in pcm.chunks(frame_samples(sample_rate)) {
            let _ = tx.send(chunk.to_vec());
        }
        drop(tx); // finite: sender closed so the lane ends when drained
        slot.mixer
            .lock()
            .expect("mixer mutex")
            .add_finite_call(rx, cells.done);
        Some(handle)
    }

    /// Move a call's RX lane from `from`'s bus to `to`'s bus (the `set_output`
    /// path), opening `to` if needed. Returns the new bus slot id. The per-call
    /// jitter `residual` on the old bus is DROPPED — accept a ≤20 ms glitch on
    /// a live bus change (Q3). No-op-ish if `from == to` (re-registers in place).
    ///
    /// # Errors
    /// Propagates [`AudioError`] from device resolution / stream open, or
    /// returns [`AudioError::DeviceNotFound`] if the call isn't on `from`.
    pub fn move_call_to_bus(
        &mut self,
        from: &OutputId,
        to: &OutputId,
        mix_id: crate::mixer::MixCallId,
        config: StreamConfig,
    ) -> Result<crate::mixer::MixCallId, AudioError> {
        let rx = self
            .outputs
            .get(from)
            .and_then(|s| s.mixer.lock().ok().and_then(|mut m| m.take_call(mix_id)))
            .ok_or_else(|| AudioError::DeviceNotFound(from.as_str().to_string()))?;
        self.ensure_output(to, config)?;
        Ok(self
            .outputs
            .get(to)
            .expect("output bus just ensured present")
            .mixer
            .lock()
            .expect("mixer mutex poisoned")
            .add_call(rx))
    }

    /// Add a caller-supplied RX `Receiver` to an output bus mixer, opening the
    /// bus if needed (the `Manager::adopt` path, where the inbound leg already
    /// owns its RX channel). Returns the bus slot id.
    ///
    /// # Errors
    /// Propagates [`AudioError`] from device resolution / stream open.
    pub fn add_rx_to_bus(
        &mut self,
        out: &OutputId,
        rx: Receiver<Vec<i16>>,
        config: StreamConfig,
    ) -> Result<crate::mixer::MixCallId, AudioError> {
        self.ensure_output(out, config)?;
        Ok(self
            .outputs
            .get(out)
            .expect("output bus just ensured present")
            .mixer
            .lock()
            .expect("mixer mutex poisoned")
            .add_call(rx))
    }

    /// Detach a call's RX lane from its bus and return its RX `Receiver` so it
    /// can be handed to the conference engine (the handset→conference live switch,
    /// iax-647d). Drops the per-call jitter `residual` (≤20 ms glitch, Q3).
    /// `None` if the bus or lane is gone.
    pub fn take_from_bus(
        &mut self,
        out: &OutputId,
        mix_id: crate::mixer::MixCallId,
    ) -> Option<Receiver<Vec<i16>>> {
        self.outputs
            .get(out)
            .and_then(|s| s.mixer.lock().ok().and_then(|mut m| m.take_call(mix_id)))
    }

    /// Remove a call's RX lane from its bus (hangup teardown). No-op if the bus
    /// or lane is gone.
    pub fn remove_from_bus(&mut self, out: &OutputId, mix_id: crate::mixer::MixCallId) {
        if let Some(slot) = self.outputs.get(out)
            && let Ok(mut m) = slot.mixer.lock()
        {
            m.remove_call(mix_id);
        }
    }

    /// Open the output bus for `out` if it isn't already open.
    fn ensure_output(&mut self, out: &OutputId, config: StreamConfig) -> Result<(), AudioError> {
        if self.outputs.contains_key(out) {
            return Ok(());
        }
        let out_dev = self.resolve(out.as_str(), Direction::Output)?;
        let mixer = Arc::new(Mutex::new(Mixer::new()));
        let bus = OutputBus::new(Arc::clone(&mixer), config.sample_rate);
        let rx_peak = bus.rx_peak();
        let rx_gain = bus.rx_gain();
        let rx_spectrum = bus.rx_spectrum();
        let rx_compress = bus.rx_compress();
        let rx_compress_level = bus.rx_compress_level();
        let handle = self.backend.open_output(&out_dev, config, Box::new(bus))?;
        self.outputs.insert(
            out.clone(),
            OutSlot {
                handle,
                mixer,
                rx_gain,
                rx_peak,
                rx_spectrum,
                rx_compress,
                rx_compress_level,
            },
        );
        Ok(())
    }

    /// Open the mic lane for `mic` if it isn't already open.
    fn ensure_mic(&mut self, mic: &MicId, config: StreamConfig) -> Result<(), AudioError> {
        if self.mics.contains_key(mic) {
            return Ok(());
        }
        let in_dev = self.resolve(mic.as_str(), Direction::Input)?;
        let dest: MicDest = Arc::new(Mutex::new(None));
        let gate = Arc::new(AtomicBool::new(false));
        let profile = Arc::new(Mutex::new(None));
        let profile_gen = Arc::new(AtomicU32::new(0));
        let inject: InjectCell = Arc::new(Mutex::new(None));
        let lane = MicLane::new(
            config.sample_rate,
            Arc::clone(&dest),
            Arc::clone(&gate),
            Arc::clone(&profile),
            Arc::clone(&profile_gen),
            Arc::clone(&inject),
        );
        // The lane owns gain/denoise/compress cells internally; fetch them so the
        // slot (and thus the router accessors) drive the same atomics.
        let gain = lane.gain_cell();
        let denoise = lane.denoise_flag();
        let compress = lane.compress_flag();
        let compress_level = lane.compress_level_cell();
        let tx_trim = lane.tx_trim_cell();
        let tx_peak = lane.tx_peak();
        let tx_spectrum = lane.tx_spectrum();
        let mic_input_peak = lane.mic_input_peak();
        let preroll_ms = lane.preroll_ms_cell();
        let overruns = Arc::new(AtomicU64::new(0));
        let handle =
            self.backend
                .open_input(&in_dev, config, Box::new(lane), Arc::clone(&overruns))?;
        self.mics.insert(
            mic.clone(),
            MicSlot {
                handle,
                dest,
                gate,
                gain,
                denoise,
                compress,
                compress_level,
                tx_trim,
                profile,
                profile_gen,
                tx_peak,
                tx_spectrum,
                mic_input_peak,
                preroll_ms,
                overruns,
                inject,
            },
        );
        Ok(())
    }

    /// Open a call with caller-supplied sink/source (the `astar-iax` Phase-1
    /// path, where the µ-law sink needs the per-call PTT gate + run-loop waker
    /// built in `dial`). The router owns the resulting stream handles so the
    /// streams outlive the call thread. The injected sink/source already do
    /// their own framing/DSP, so the lane's `dest`/`mixer` cells are left
    /// inert for these slots.
    ///
    /// # Errors
    /// Propagates [`AudioError`] from device resolution / stream open.
    pub fn open_call_with(
        &mut self,
        mic: &MicId,
        out: &OutputId,
        config: StreamConfig,
        sink: Box<dyn InputSink>,
        source: Box<dyn OutputSource>,
    ) -> Result<(), AudioError> {
        let in_dev = self.resolve(mic.as_str(), Direction::Input)?;
        let out_dev = self.resolve(out.as_str(), Direction::Output)?;
        let overruns = Arc::new(AtomicU64::new(0));
        let in_handle = self
            .backend
            .open_input(&in_dev, config, sink, Arc::clone(&overruns))?;
        let out_handle = self.backend.open_output(&out_dev, config, source)?;
        self.mics.insert(
            mic.clone(),
            MicSlot {
                handle: in_handle,
                dest: Arc::new(Mutex::new(None)),
                gate: Arc::new(AtomicBool::new(false)),
                gain: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
                denoise: Arc::new(AtomicBool::new(false)),
                compress: Arc::new(AtomicBool::new(false)),
                compress_level: Arc::new(AtomicU32::new(DEFAULT_COMPRESS_LEVEL.to_bits())),
                tx_trim: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
                profile: Arc::new(Mutex::new(None)),
                profile_gen: Arc::new(AtomicU32::new(0)),
                tx_peak: Arc::new(AtomicU32::new(0)),
                tx_spectrum: Arc::new(Mutex::new(SpectrumAnalyzer::new(config.sample_rate))),
                mic_input_peak: Arc::new(AtomicU32::new(0)),
                preroll_ms: Arc::new(AtomicU32::new(0)),
                overruns,
                inject: Arc::new(Mutex::new(None)),
            },
        );
        self.outputs.insert(
            out.clone(),
            OutSlot {
                handle: out_handle,
                mixer: Arc::new(Mutex::new(Mixer::new())),
                rx_gain: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
                rx_peak: Arc::new(AtomicU32::new(0)),
                rx_spectrum: Arc::new(Mutex::new(SpectrumAnalyzer::new(config.sample_rate))),
                rx_compress: Arc::new(AtomicBool::new(false)),
                rx_compress_level: Arc::new(AtomicU32::new(DEFAULT_COMPRESS_LEVEL.to_bits())),
            },
        );
        Ok(())
    }

    /// Open a mic with a caller-supplied [`InputSink`] (the monitor-mode path,
    /// iax-2377). Unlike [`AudioRouter::ensure_mic`] this does not build a
    /// `MicLane` (no encode/TX/PTT) — the injected sink is a pure observer
    /// (spectrum + capture). The router owns the resulting stream handle so the
    /// stream outlives the caller, and the lane's `dest`/`gate`/profile cells
    /// are left inert for this slot. Errors if the mic is already open.
    ///
    /// # Errors
    /// Propagates [`AudioError`] from device resolution / stream open.
    pub fn open_input_with(
        &mut self,
        mic: &MicId,
        config: StreamConfig,
        sink: Box<dyn InputSink>,
    ) -> Result<(), AudioError> {
        let in_dev = self.resolve(mic.as_str(), Direction::Input)?;
        let overruns = Arc::new(AtomicU64::new(0));
        let handle = self
            .backend
            .open_input(&in_dev, config, sink, Arc::clone(&overruns))?;
        self.mics.insert(
            mic.clone(),
            MicSlot {
                handle,
                dest: Arc::new(Mutex::new(None)),
                gate: Arc::new(AtomicBool::new(false)),
                gain: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
                denoise: Arc::new(AtomicBool::new(false)),
                compress: Arc::new(AtomicBool::new(false)),
                compress_level: Arc::new(AtomicU32::new(DEFAULT_COMPRESS_LEVEL.to_bits())),
                tx_trim: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
                profile: Arc::new(Mutex::new(None)),
                profile_gen: Arc::new(AtomicU32::new(0)),
                tx_peak: Arc::new(AtomicU32::new(0)),
                tx_spectrum: Arc::new(Mutex::new(SpectrumAnalyzer::new(config.sample_rate))),
                mic_input_peak: Arc::new(AtomicU32::new(0)),
                preroll_ms: Arc::new(AtomicU32::new(0)),
                overruns,
                inject: Arc::new(Mutex::new(None)),
            },
        );
        Ok(())
    }

    #[must_use]
    pub fn mic_count(&self) -> usize {
        self.mics.len()
    }
    #[must_use]
    pub fn output_count(&self) -> usize {
        self.outputs.len()
    }

    /// Number of calls mixed onto the given output bus (0 if not open).
    #[must_use]
    pub fn bus_call_count(&self, out: &OutputId) -> usize {
        self.outputs
            .get(out)
            .map_or(0, |s| s.mixer.lock().map_or(0, |m| m.call_count()))
    }

    // --- per-mic controls (None if the mic isn't open) ---

    /// Set the capture gain on an open mic lane. No-op if the mic isn't open.
    pub fn set_mic_gain(&self, mic: &MicId, g: f32) {
        if let Some(s) = self.mics.get(mic) {
            s.gain.store(g.to_bits(), Ordering::Relaxed);
        }
    }
    /// Current capture gain on an open mic lane.
    #[must_use]
    pub fn mic_gain(&self, mic: &MicId) -> Option<f32> {
        self.mics
            .get(mic)
            .map(|s| f32::from_bits(s.gain.load(Ordering::Relaxed)))
    }
    /// Toggle noise reduction on an open mic lane. No-op if the mic isn't open.
    pub fn set_mic_denoise(&self, mic: &MicId, on: bool) {
        if let Some(s) = self.mics.get(mic) {
            s.denoise.store(on, Ordering::Relaxed);
        }
    }
    /// Toggle the compressor on an open mic lane. No-op if the mic isn't open.
    pub fn set_mic_compress(&self, mic: &MicId, on: bool) {
        if let Some(s) = self.mics.get(mic) {
            s.compress.store(on, Ordering::Relaxed);
        }
    }
    /// Set the compressor strength (0.0..=1.0, clamped) on an open mic lane. The
    /// lane re-tunes live on the audio thread. No-op if the mic isn't open.
    pub fn set_mic_compress_level(&self, mic: &MicId, level: f32) {
        if let Some(s) = self.mics.get(mic) {
            s.compress_level
                .store(level.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }
    /// Set the TX trim (0.0..=2.0, clamped; 1.0 = unity) on an open mic lane:
    /// the always-on final gain stage after the compressor (iax-750a). No-op if
    /// the mic isn't open.
    pub fn set_mic_tx_trim(&self, mic: &MicId, g: f32) {
        if let Some(s) = self.mics.get(mic) {
            s.tx_trim
                .store(g.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
        }
    }
    /// Set the VOX pre-roll / look-back length (ms, clamped to
    /// `0..=MAX_PREROLL_MS`) on an open mic lane (iax-2733). `0` disables
    /// pre-roll (the lane keeps the cheap unkeyed early-return). No-op if the
    /// mic isn't open.
    pub fn set_mic_preroll_ms(&self, mic: &MicId, ms: u32) {
        if let Some(s) = self.mics.get(mic) {
            s.preroll_ms
                .store(ms.min(MAX_PREROLL_MS), Ordering::Relaxed);
        }
    }
    /// Current VOX pre-roll length (ms) on an open mic lane (iax-2733).
    #[must_use]
    pub fn mic_preroll_ms(&self, mic: &MicId) -> Option<u32> {
        self.mics
            .get(mic)
            .map(|s| s.preroll_ms.load(Ordering::Relaxed))
    }
    /// Cumulative cpal capture overruns on an open mic's input stream
    /// (iax-9e55): dropped input buffers (holes in the captured PCM), the lead
    /// suspect for choppy TX. `None` if the mic isn't open. A plain `u64` health
    /// counter, credential-free.
    #[must_use]
    pub fn mic_capture_overruns(&self, mic: &MicId) -> Option<u64> {
        self.mics
            .get(mic)
            .map(|s| s.overruns.load(Ordering::Relaxed))
    }
    /// Clone an open mic's cumulative capture-overrun cell (iax-9e55) so a
    /// consumer (e.g. the `Manager` binding it into a `Call`'s snapshot) can read
    /// it live. `None` if the mic isn't open. The cell is a plain `Arc<AtomicU64>`
    /// health counter, credential-free.
    #[must_use]
    pub fn mic_overruns_cell(&self, mic: &MicId) -> Option<Arc<AtomicU64>> {
        self.mics.get(mic).map(|s| Arc::clone(&s.overruns))
    }
    /// Push a calibrated per-mic profile onto an open lane (rebuilds its noise
    /// reducer on the audio thread next `write`). No-op if the mic isn't open.
    pub fn set_mic_profile(&self, mic: &MicId, profile: Option<MicProfile>) {
        if let Some(s) = self.mics.get(mic) {
            *s.profile.lock().expect("profile mutex") = profile;
            s.profile_gen.fetch_add(1, Ordering::Relaxed);
        }
    }
    /// Current TX level on an open mic lane in dBFS (post-DSP peak).
    #[must_use]
    pub fn mic_tx_dbfs(&self, mic: &MicId) -> Option<f32> {
        self.mics
            .get(mic)
            .map(|s| crate::peak_to_dbfs(f32::from_bits(s.tx_peak.load(Ordering::Relaxed))))
    }
    /// Copy the live TX spectrum (iax-2b09) of an open mic lane into `out_bins`
    /// (the SAME log-binned, peak-held dBFS values the mic monitor produces) and
    /// return the number of bins written. `None` if the mic isn't open. The lane
    /// feeds this analyzer the post-DSP, pre-encode TX PCM — a pure observer that
    /// never perturbs the outgoing call audio.
    #[must_use]
    pub fn mic_tx_spectrum(&self, mic: &MicId, out_bins: &mut [f32]) -> Option<usize> {
        self.mics
            .get(mic)
            .map(|s| s.tx_spectrum.lock().map_or(0, |sa| sa.copy_into(out_bins)))
    }
    /// Set the live TX spectrum peak-hold decay (dB/SECOND, clamped, iax-8616)
    /// on an open mic lane. Takes effect immediately on the lane's analyzer.
    /// No-op if the mic isn't open.
    pub fn set_mic_spectrum_decay(&self, mic: &MicId, db_per_sec: f32) {
        if let Some(s) = self.mics.get(mic)
            && let Ok(mut sa) = s.tx_spectrum.lock()
        {
            sa.set_decay_db_per_sec(db_per_sec);
        }
    }
    /// Current mic INPUT level on an open lane in dBFS (post-gain, pre-NR peak),
    /// metered CONTINUOUSLY even while unkeyed so VOX can key from silence
    /// (iax-5c30). Unlike `mic_tx_dbfs`, this does not floor when the PTT gate
    /// is closed.
    #[must_use]
    pub fn mic_input_dbfs(&self, mic: &MicId) -> Option<f32> {
        self.mics
            .get(mic)
            .map(|s| crate::peak_to_dbfs(f32::from_bits(s.mic_input_peak.load(Ordering::Relaxed))))
    }

    // --- per-output controls (None if the bus isn't open) ---

    /// Set the output gain on an open bus (clamped `0.0..=4.0`: 100%..400%
    /// headroom for boosting a quiet station, iax-a4e7). No-op if the bus
    /// isn't open. `0.0` is a valid floor, not just a clamp boundary — the
    /// half-duplex RX-mute path relies on it to silence a bus outright.
    pub fn set_output_gain(&self, out: &OutputId, g: f32) {
        if let Some(s) = self.outputs.get(out) {
            s.rx_gain
                .store(g.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
        }
    }
    /// Current output gain on an open bus.
    #[must_use]
    pub fn output_gain(&self, out: &OutputId) -> Option<f32> {
        self.outputs
            .get(out)
            .map(|s| f32::from_bits(s.rx_gain.load(Ordering::Relaxed)))
    }
    /// Current RX level on an open bus in dBFS (post-mix, post-gain peak).
    #[must_use]
    pub fn output_rx_dbfs(&self, out: &OutputId) -> Option<f32> {
        self.outputs
            .get(out)
            .map(|s| crate::peak_to_dbfs(f32::from_bits(s.rx_peak.load(Ordering::Relaxed))))
    }

    /// Copy the live RX spectrum (iax-2b09) of an open bus into `out_bins` (the
    /// SAME log-binned, peak-held dBFS values the mic monitor produces) and
    /// return the number of bins written. `None` if the bus isn't open. The bus
    /// feeds this analyzer the post-mix decoded RX PCM — a pure observer that
    /// never perturbs the audio handed to the output device.
    #[must_use]
    pub fn output_rx_spectrum(&self, out: &OutputId, out_bins: &mut [f32]) -> Option<usize> {
        self.outputs
            .get(out)
            .map(|s| s.rx_spectrum.lock().map_or(0, |sa| sa.copy_into(out_bins)))
    }

    /// Set the live RX spectrum peak-hold decay (dB/SECOND, clamped, iax-8616)
    /// on an open output bus. Takes effect immediately on the bus's analyzer.
    /// No-op if the bus isn't open.
    pub fn set_output_spectrum_decay(&self, out: &OutputId, db_per_sec: f32) {
        if let Some(s) = self.outputs.get(out)
            && let Ok(mut sa) = s.rx_spectrum.lock()
        {
            sa.set_decay_db_per_sec(db_per_sec);
        }
    }

    /// Toggle RX/output compression on an open output bus (iax-a4e7 PHASE 1):
    /// reuses the mic-path [`Compressor`] (makeup gain included) on the
    /// OUTPUT bus, applied BEFORE the bus gain multiply in
    /// [`OutputBus::read`] so the 100-400% output-gain range amplifies the
    /// already-leveled signal, not the raw mix. Automatic leveling: quiet
    /// stations are brought UP toward the target via makeup gain, loud ones
    /// are squashed toward the threshold — the same DSP the mic path uses for
    /// TX, mirrored onto RX. No-op if the bus isn't open. Default OFF
    /// (byte-identical rule).
    pub fn set_output_compress(&self, out: &OutputId, on: bool) {
        if let Some(s) = self.outputs.get(out) {
            s.rx_compress.store(on, Ordering::Relaxed);
        }
    }
    /// Current RX/output compression toggle on an open bus.
    #[must_use]
    pub fn output_compress(&self, out: &OutputId) -> Option<bool> {
        self.outputs
            .get(out)
            .map(|s| s.rx_compress.load(Ordering::Relaxed))
    }
    /// Set the RX/output compression strength (0.0..=1.0, clamped) on an open
    /// bus. The bus re-tunes live on the audio thread. No-op if the bus isn't
    /// open.
    pub fn set_output_compress_level(&self, out: &OutputId, level: f32) {
        if let Some(s) = self.outputs.get(out) {
            s.rx_compress_level
                .store(level.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }
    /// Current RX/output compression strength on an open bus.
    #[must_use]
    pub fn output_compress_level(&self, out: &OutputId) -> Option<f32> {
        self.outputs
            .get(out)
            .map(|s| f32::from_bits(s.rx_compress_level.load(Ordering::Relaxed)))
    }
}

/// A throwaway `mio::Waker` for static Phase-2 wiring (the real per-call waker
/// arrives with the `Manager` in Phase 3). Falls back silently if the OS
/// can't build one — the lane just won't self-wake until a real dest is set.
fn throwaway_waker() -> Arc<mio::Waker> {
    let poll = mio::Poll::new().expect("mio Poll");
    Arc::new(mio::Waker::new(poll.registry(), mio::Token(0)).expect("mio Waker"))
}

/// The `OutputSource` for one physical output device, backed by a shared
/// [`Mixer`]. The router adds/removes call RX channels via the same `Arc`,
/// while the cpal output thread pulls summed audio through `read` (iax-42e9
/// phase 2).
pub struct OutputBus {
    mixer: Arc<Mutex<Mixer>>,
    rx_peak: Arc<AtomicU32>,
    rx_gain: Arc<AtomicU32>, // f32 bits, default 1.0 = unity
    /// Live RX spectrum (iax-2b09): the SAME log-binned, peak-held analyzer the
    /// mic monitor uses, fed the post-mix/post-gain decoded RX PCM in `read` so
    /// the harness can draw the spectrum of audio received from the node during
    /// a call. A pure observer — it never alters the audio handed to the device.
    rx_spectrum: Arc<Mutex<SpectrumAnalyzer>>,
    /// RX/output compression toggle (iax-a4e7 PHASE 1), shared with the
    /// control side; default OFF (byte-identical rule).
    rx_compress: Arc<AtomicBool>,
    /// RX/output compression strength 0.0..=1.0 (f32 bits, default 0.90),
    /// shared with the control side; the bus re-tunes `comp` on the audio
    /// thread when it changes.
    rx_compress_level: Arc<AtomicU32>,
    /// Bits of the last compress level applied to `comp` (audio-thread-private;
    /// compared as bits so an unchanged cell is a cheap, exact no-op).
    applied_compress_level_bits: u32,
    /// The reused mic-path compressor, applied to the OUTPUT bus's POST-MIX
    /// (summed) audio for automatic RX leveling (iax-a4e7 PHASE 1) — this bus
    /// may carry more than one call's RX (`Mixer` sums every routed call
    /// before `read` ever sees it), so the compressor levels the crowd as a
    /// whole, not any one station individually. Owned by this bus (not
    /// shared) — only the cpal output thread ever calls `read`, exactly like
    /// `MicLane` owns its `comp` for the capture side.
    comp: Compressor,
    /// Sample rate this bus's `comp` is tuned for (needed to re-tune on a
    /// live level change).
    sample_rate: u32,
}

impl OutputBus {
    /// Build a bus over a shared mixer; the router keeps the same `Arc` to
    /// add/remove call RX channels live. `sample_rate` seeds the RX spectrum
    /// analyzer (iax-2b09) and the RX compressor (iax-a4e7 PHASE 1).
    #[must_use]
    pub fn new(mixer: Arc<Mutex<Mixer>>, sample_rate: u32) -> Self {
        Self {
            mixer,
            rx_peak: Arc::new(AtomicU32::new(0)),
            rx_gain: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            rx_spectrum: Arc::new(Mutex::new(SpectrumAnalyzer::new(sample_rate))),
            rx_compress: Arc::new(AtomicBool::new(false)),
            rx_compress_level: Arc::new(AtomicU32::new(DEFAULT_COMPRESS_LEVEL.to_bits())),
            applied_compress_level_bits: DEFAULT_COMPRESS_LEVEL.to_bits(),
            comp: Compressor::with_params(
                sample_rate,
                CompressorParams::for_level(DEFAULT_COMPRESS_LEVEL),
            ),
            sample_rate,
        }
    }

    /// Shared RX-peak cell for per-bus metering (f32 bits in an `AtomicU32`).
    #[must_use]
    pub fn rx_peak(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.rx_peak)
    }

    /// Shared output-gain cell for this bus (f32 bits, default 1.0 = unity).
    #[must_use]
    pub fn rx_gain(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.rx_gain)
    }

    /// Shared RX spectrum analyzer cell for this bus (iax-2b09).
    #[must_use]
    pub fn rx_spectrum(&self) -> Arc<Mutex<SpectrumAnalyzer>> {
        Arc::clone(&self.rx_spectrum)
    }

    /// Shared RX/output compression toggle for this bus (iax-a4e7 PHASE 1).
    #[must_use]
    pub fn rx_compress(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.rx_compress)
    }

    /// Shared RX/output compression strength cell for this bus (f32 bits,
    /// 0.0..=1.0, default 0.90; iax-a4e7 PHASE 1).
    #[must_use]
    pub fn rx_compress_level(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.rx_compress_level)
    }
}

impl OutputSource for OutputBus {
    fn read(&mut self, out: &mut [f32]) -> usize {
        let n = self.mixer.lock().map_or(0, |mut m| m.read(out));
        // iax-a4e7 PHASE 1: RX compression — reuse the mic-path Compressor
        // (makeup gain included) on the OUTPUT bus's POST-MIX (summed) audio
        // — `m.read` above already sums every call routed to this bus, so
        // this levels the crowd as a whole — applied BEFORE the bus gain
        // multiply below so the 100-400% output-gain range amplifies the
        // already-leveled signal rather than the raw mix. Off by default
        // (byte-identical rule).
        if self.rx_compress.load(Ordering::Relaxed) {
            let lvl_bits = self.rx_compress_level.load(Ordering::Relaxed);
            if lvl_bits != self.applied_compress_level_bits {
                self.comp.set_params(
                    self.sample_rate,
                    CompressorParams::for_level(f32::from_bits(lvl_bits)),
                );
                self.applied_compress_level_bits = lvl_bits;
            }
            self.comp.process(&mut out[..n]);
        }
        // Apply output gain before metering so the peak reflects post-gain audio.
        let g = f32::from_bits(self.rx_gain.load(Ordering::Relaxed));
        if (g - 1.0).abs() >= f32::EPSILON {
            for s in &mut out[..n] {
                *s = (*s * g).clamp(-1.0, 1.0);
            }
        }
        // Store the post-mix peak so a UI can show per-bus RX level.
        self.rx_peak
            .store(peak(&out[..n]).to_bits(), Ordering::Relaxed);
        // Feed the live RX spectrum (iax-2b09) the FULL buffer, zeroing the tail the
        // mixer underran. A peak-hold only decays when it's fed, so feeding only the
        // `n` real samples would FREEZE the spectrum the moment RX stops (the mixer
        // returns 0 on underrun); feeding silence keeps it decaying to the floor.
        // Zeroing the tail also makes the device play clean silence on underrun
        // rather than stale buffer content.
        out[n..].fill(0.0);
        if let Ok(mut sa) = self.rx_spectrum.lock() {
            sa.push(out);
        }
        n
    }
}

/// One voice frame is 20 ms (160 PCM samples at 8 kHz).
pub const VOICE_FRAME_MS: u32 = 20;

/// Samples per 20 ms voice frame at `sample_rate` (160 @ 8 kHz, 320 @ 16 kHz).
/// Wire byte size is per-format (`VoiceFormat::frame_bytes_20ms`).
#[must_use]
pub const fn frame_samples(sample_rate: u32) -> usize {
    (sample_rate / (1000 / VOICE_FRAME_MS)) as usize
}

/// Maximum VOX pre-roll / look-back the mic lane will retain and flush on
/// key-up (iax-2733). The look-back ring is sized to this bound; `preroll_ms`
/// selects how much of it to flush. 250 ms = 13 frames of 20 ms.
pub const MAX_PREROLL_MS: u32 = 250;

/// Capacity (in 20 ms frames) of the look-back ring at the maximum pre-roll.
/// `MAX_PREROLL_MS / VOICE_FRAME_MS`, rounded up. 250 / 20 = 12.5 → 13.
pub const MAX_PREROLL_FRAMES: usize = (MAX_PREROLL_MS as usize).div_ceil(VOICE_FRAME_MS as usize);

/// One mic-lane destination: the call's TX `Sender`, its run-loop `Waker`, and
/// its VOX pre-roll lead cell (iax-2733) — the three travel together so a live
/// re-route points the lane at the *new* call's sender, waker, AND lead cell
/// (iax-158c fix, extended). `None` = not routed (monitor-only / no call).
pub type MicDestUnit = (Sender<Vec<i16>>, Arc<mio::Waker>, Arc<AtomicU32>);

/// Shared current destination for a mic lane, swapped together on `route()`
/// (Q1, accepted).
pub type MicDest = Arc<Mutex<Option<MicDestUnit>>>;

/// One queued announcement on a mic lane (iax-e30d). The audio thread drains
/// `pcm` (8 kHz f32), flips `cells.done` when empty, and honors `cells.cancel`.
pub(crate) struct Inject {
    pub(crate) pcm: std::collections::VecDeque<f32>,
    pub(crate) policy: crate::AnnouncePolicy,
    pub(crate) cells: crate::announce::AnnounceCells,
}

/// Shared current announcement on a mic lane (`None` = none playing).
pub(crate) type InjectCell = Arc<Mutex<Option<Inject>>>;

/// One capture device's DSP + framing path. Runs on the cpal input thread.
///
/// Chain: gain → `NoiseReducer` → `Compressor` → f32→i16 PCM → 20 ms
/// framing (re-homed from `metering.rs` + `audio_bridge.rs`), gated by a PTT
/// flag, and writes PCM frames into a **swappable current destination** (the
/// codec transcode is at the network edge, iax-31f7) — only while
/// Default compressor strength when a mic lane opens (iax-d9bb). 0.90 = punchy
/// radio voice; the control side adjusts it live via `set_mic_compress_level`.
const DEFAULT_COMPRESS_LEVEL: f32 = 0.90;

/// keyed. Phase 3's `route()` swaps the `(Sender, Waker)` unit.
pub struct MicLane {
    dest: MicDest,
    gate: Arc<AtomicBool>,
    sample_rate: u32,
    /// Samples per 20 ms frame at `sample_rate` (`frame_samples(sample_rate)`),
    /// cached so the hot path doesn't recompute it (iax-4348).
    frame_samples: usize,
    nr: NoiseReducer,
    comp: Compressor,
    denoise: Arc<AtomicBool>,
    compress: Arc<AtomicBool>,
    /// Compressor strength 0.0..=1.0 (f32 bits, default 0.90), shared with the
    /// control side; the lane re-tunes `comp` on the audio thread when it changes.
    compress_level: Arc<AtomicU32>,
    /// Bits of the last compress level applied to `comp` (audio-thread-private;
    /// compared as bits so an unchanged cell is a cheap, exact no-op).
    applied_compress_level_bits: u32,
    /// TX trim 0.0..=2.0 (f32 bits, default 1.0 = unity), shared with the
    /// control side (iax-750a). The always-on FINAL gain stage, applied after
    /// the compressor (so it defeats makeup gain) and before the TX peak /
    /// spectrum taps (meters show what is transmitted). A plain multiply — no
    /// retuning, so no `applied` shadow is needed.
    tx_trim: Arc<AtomicU32>,
    gain: Arc<AtomicU32>, // f32 bits, default 1.0
    tx_peak: Arc<AtomicU32>,
    /// Live TX spectrum (iax-2b09): the SAME log-binned, peak-held analyzer the
    /// mic monitor uses, fed the post-DSP f32 PCM in `dsp_quantize` so a harness
    /// can draw the outgoing-audio spectrum during a call. A pure observer — it
    /// never alters the TX stream.
    tx_spectrum: Arc<Mutex<SpectrumAnalyzer>>,
    /// Mic INPUT peak (f32 bits): post-gain, pre-NoiseReducer, metered every
    /// `write` regardless of the gate, so VOX can key from silence (iax-5c30).
    mic_input_peak: Arc<AtomicU32>,
    /// Calibrated per-mic profile, swapped off-thread; the audio thread rebuilds
    /// `nr` from it when `profile_gen` advances past `seen_gen`.
    profile: Arc<Mutex<Option<MicProfile>>>,
    /// Generation counter bumped by the control side on each profile swap.
    profile_gen: Arc<AtomicU32>,
    /// Last generation this lane applied (audio-thread-private; never shared).
    seen_gen: u32,
    buf_f32: Vec<f32>,
    buf_pcm: Vec<i16>,
    /// VOX pre-roll length in ms (iax-2733), shared with the control side.
    /// `0` = disabled: the lane keeps today's cheap unkeyed early-return.
    preroll_ms: Arc<AtomicU32>,
    /// Look-back ring of post-DSP PCM frames retained while unkeyed (Q1: the
    /// buffered audio is the same shape as the live transmitted stream). Sized
    /// to `MAX_PREROLL_FRAMES`; the newest `preroll_ms / 20` frames are flushed
    /// ahead of the live stream on key-up. Audio-thread-private.
    preroll_ring: std::collections::VecDeque<Vec<i16>>,
    /// Previous gate state (audio-thread-private) for FALSE→TRUE edge detection.
    prev_gate: bool,
    /// Active TX announcement injected ahead of the mic (iax-e30d).
    inject: InjectCell,
}

impl MicLane {
    /// Build a lane for one capture device feeding the swappable `dest`,
    /// gated by `gate` (closed = monitor-only / unkeyed). `profile` +
    /// `profile_gen` are the shared cells the control side uses to push a
    /// calibrated per-mic profile onto the live lane; the lane rebuilds its
    /// `NoiseReducer` on the audio thread when the generation advances.
    /// `inject` is the shared cell for TX announcements (iax-e30d).
    #[must_use]
    pub(crate) fn new(
        sample_rate: u32,
        dest: MicDest,
        gate: Arc<AtomicBool>,
        profile: Arc<Mutex<Option<MicProfile>>>,
        profile_gen: Arc<AtomicU32>,
        inject: InjectCell,
    ) -> Self {
        // Seed the NR from the profile's current value (if any).
        let nr = match profile.lock().expect("profile mutex").as_ref() {
            Some(p) => NoiseReducer::from_profile(sample_rate, p),
            None => NoiseReducer::new(sample_rate),
        };
        Self {
            dest,
            gate,
            sample_rate,
            frame_samples: frame_samples(sample_rate),
            nr,
            comp: Compressor::with_params(
                sample_rate,
                CompressorParams::for_level(DEFAULT_COMPRESS_LEVEL),
            ),
            denoise: Arc::new(AtomicBool::new(false)),
            compress: Arc::new(AtomicBool::new(false)),
            compress_level: Arc::new(AtomicU32::new(DEFAULT_COMPRESS_LEVEL.to_bits())),
            applied_compress_level_bits: DEFAULT_COMPRESS_LEVEL.to_bits(),
            tx_trim: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            gain: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            tx_peak: Arc::new(AtomicU32::new(0)),
            tx_spectrum: Arc::new(Mutex::new(SpectrumAnalyzer::new(sample_rate))),
            mic_input_peak: Arc::new(AtomicU32::new(0)),
            profile,
            profile_gen,
            seen_gen: 0,
            buf_f32: Vec::with_capacity(2 * frame_samples(sample_rate)),
            buf_pcm: Vec::with_capacity(2 * frame_samples(sample_rate)),
            preroll_ms: Arc::new(AtomicU32::new(0)),
            preroll_ring: std::collections::VecDeque::with_capacity(MAX_PREROLL_FRAMES),
            prev_gate: false,
            inject,
        }
    }

    /// Shared TX-peak cell for per-lane metering (f32 bits in an `AtomicU32`).
    #[must_use]
    pub fn tx_peak(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.tx_peak)
    }
    /// Shared live TX spectrum analyzer cell for this lane (iax-2b09).
    #[must_use]
    pub fn tx_spectrum(&self) -> Arc<Mutex<SpectrumAnalyzer>> {
        Arc::clone(&self.tx_spectrum)
    }
    /// Shared mic-INPUT-peak cell (f32 bits): continuous post-gain, pre-NR level
    /// for VOX (iax-5c30).
    #[must_use]
    pub fn mic_input_peak(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.mic_input_peak)
    }
    /// Toggle cell for the noise reducer (control side).
    #[must_use]
    pub fn denoise_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.denoise)
    }
    /// Toggle cell for the compressor (control side).
    #[must_use]
    pub fn compress_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.compress)
    }
    /// Compressor strength cell (control side; f32 bits, 0.0..=1.0, default 0.90).
    #[must_use]
    pub fn compress_level_cell(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.compress_level)
    }
    /// TX trim cell (control side; f32 bits, 0.0..=2.0, default 1.0 = unity):
    /// the always-on final gain stage after the compressor (iax-750a).
    #[must_use]
    pub fn tx_trim_cell(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.tx_trim)
    }
    /// Capture-gain cell (f32 bits, default 1.0).
    #[must_use]
    pub fn gain_cell(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.gain)
    }
    /// VOX pre-roll length cell (ms; control side, default 0 = disabled,
    /// clamped to `0..=MAX_PREROLL_MS`).
    #[must_use]
    pub fn preroll_ms_cell(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.preroll_ms)
    }

    /// Rebuild the noise reducer from a calibrated per-mic profile (per-mic
    /// notches), or reset to the generic filter when `None`. Called off the audio
    /// thread while idle / between calls; do not race a live `write`.
    pub fn set_profile(&mut self, profile: Option<&MicProfile>) {
        self.nr = match profile {
            Some(p) => NoiseReducer::from_profile(self.sample_rate, p),
            None => NoiseReducer::new(self.sample_rate),
        };
    }

    /// Run the full capture DSP chain (gain → NR → compress → i16 PCM) on
    /// `samples`, appending the quantized PCM samples to `self.buf_pcm` and
    /// publishing the post-DSP TX peak. Shared by the live (keyed) path and the
    /// unkeyed pre-roll capture path so both buffer exactly the same shape (Q1).
    fn dsp_quantize(&mut self, samples: &[f32], g: f32) {
        self.buf_f32.clear();
        if (g - 1.0).abs() < f32::EPSILON {
            self.buf_f32.extend_from_slice(samples);
        } else {
            self.buf_f32
                .extend(samples.iter().map(|&s| (s * g).clamp(-1.0, 1.0)));
        }
        if self.denoise.load(Ordering::Relaxed) {
            self.nr.process(&mut self.buf_f32);
        }
        if self.compress.load(Ordering::Relaxed) {
            // Re-tune to the current level if the control side changed it (rare;
            // a cheap exact bits compare on the hot path). Keeps envelope state.
            let lvl_bits = self.compress_level.load(Ordering::Relaxed);
            if lvl_bits != self.applied_compress_level_bits {
                self.comp.set_params(
                    self.sample_rate,
                    CompressorParams::for_level(f32::from_bits(lvl_bits)),
                );
                self.applied_compress_level_bits = lvl_bits;
            }
            self.comp.process(&mut self.buf_f32);
        }
        // iax-750a: TX trim — the always-on FINAL gain stage, after the
        // compressor (so it defeats makeup gain) and before the peak/spectrum
        // taps (meters show what is transmitted). Unity is skipped, same idiom
        // as the input-gain stage above.
        let trim = f32::from_bits(self.tx_trim.load(Ordering::Relaxed));
        if (trim - 1.0).abs() >= f32::EPSILON {
            for s in &mut self.buf_f32 {
                *s = (*s * trim).clamp(-1.0, 1.0);
            }
        }
        self.tx_peak
            .store(peak(&self.buf_f32).to_bits(), Ordering::Relaxed);
        // Feed the live TX spectrum (iax-2b09) with the post-DSP f32 PCM — the
        // same audio about to be quantized to i16 and sent — as a pure observer
        // that never alters the outgoing stream.
        if let Ok(mut sa) = self.tx_spectrum.lock() {
            sa.push(&self.buf_f32);
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_possible_wrap
        )]
        self.buf_pcm.extend(
            self.buf_f32
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16),
        );
    }
}

impl InputSink for MicLane {
    #[allow(clippy::too_many_lines)] // iax-e30d inject block added; extract helper in follow-on
    fn write(&mut self, samples: &[f32], _meter: f32) {
        // Cheap atomic load on the hot path; only locks + rebuilds the NR when
        // the control side pushed a new profile (generation advanced).
        let cur_gen = self.profile_gen.load(Ordering::Relaxed);
        if cur_gen != self.seen_gen {
            let p = self.profile.lock().expect("profile mutex");
            self.nr = match p.as_ref() {
                Some(p) => NoiseReducer::from_profile(self.sample_rate, p),
                None => NoiseReducer::new(self.sample_rate),
            };
            self.seen_gen = cur_gen;
        }
        // Meter the mic INPUT level CONTINUOUSLY — post-gain, pre-NoiseReducer,
        // and BEFORE the unkeyed early-return — so a consumer's VOX can key from
        // silence (iax-5c30). Tap point rides the input-gain slider (VOX
        // sensitivity) and precedes the NR noise-gate (which would otherwise
        // suppress the soft speech onset VOX must detect).
        let g = f32::from_bits(self.gain.load(Ordering::Relaxed));
        let input_peak = (g * peak(samples)).min(1.0);
        self.mic_input_peak
            .store(input_peak.to_bits(), Ordering::Relaxed);

        let keyed = self.gate.load(Ordering::Relaxed);
        // Pre-roll length (frames). 0 = disabled → keep today's cheap path.
        #[allow(clippy::cast_possible_truncation)]
        let preroll_frames = (self.preroll_ms.load(Ordering::Relaxed) / VOICE_FRAME_MS) as usize;

        if !keyed {
            self.prev_gate = false;
            self.buf_pcm.clear(); // drop partial tail on unkey
            if preroll_frames == 0 {
                // iax-2733: pre-roll disabled — the byte-for-byte legacy unkeyed
                // early-return. No ring, no unkeyed DSP, no allocation.
                self.preroll_ring.clear();
                return;
            }
            // iax-2733: pre-roll enabled — run the SAME DSP+encode the live path
            // uses (Q1: buffer post-DSP PCM so the onset is the same shape as
            // the transmitted stream) and retain the newest frames in the ring
            // INSTEAD of sending to dest. Cap to the configured pre-roll length.
            self.dsp_quantize(samples, g);
            while self.buf_pcm.len() >= self.frame_samples {
                let frame: Vec<i16> = self.buf_pcm.drain(..self.frame_samples).collect();
                if self.preroll_ring.len() == preroll_frames.min(MAX_PREROLL_FRAMES) {
                    self.preroll_ring.pop_front();
                }
                self.preroll_ring.push_back(frame);
            }
            self.buf_pcm.clear(); // drop any sub-frame tail
            return;
        }

        // Keyed. Snapshot the current destination (Sender + Waker, swapped
        // together by route() — Q1). Wake the *current* call's loop after
        // sending so voice flows promptly on a quiet link, across a live re-route.
        let dest = self.dest.lock().unwrap().clone();

        // FALSE→TRUE gate edge (key-up): flush the look-back ring AHEAD of the
        // live stream so the buffered speech onset isn't clipped (iax-2733).
        // Signal the run-loop (via the lead cell that travels with the dest) how
        // many frames lead the live stream so its media-clock ladder absorbs the
        // deliberate lead instead of re-anchoring.
        if !self.prev_gate {
            self.prev_gate = true;
            if preroll_frames > 0 {
                // Keep only the newest `preroll_frames` of the ring.
                while self.preroll_ring.len() > preroll_frames {
                    self.preroll_ring.pop_front();
                }
                if let Some((tx, _, lead)) = dest.as_ref() {
                    let flushed = self.preroll_ring.len();
                    if flushed > 0 {
                        // Publish the lead BEFORE the frames land so the run-loop
                        // sees it whichever order it polls (cell then channel).
                        #[allow(clippy::cast_possible_truncation)]
                        lead.fetch_add(flushed as u32, Ordering::Relaxed);
                        for frame in self.preroll_ring.drain(..) {
                            let _ = tx.send(frame);
                        }
                    }
                }
            }
            self.preroll_ring.clear();
        }

        // iax-e30d: apply a pending announcement. Seize replaces the captured
        // frame; MixUnder sums the announcement over it at the resolved gain.
        let mut work: std::borrow::Cow<[f32]> = std::borrow::Cow::Borrowed(samples);
        {
            let mut slot = self.inject.lock().expect("inject mutex");
            if let Some(inj) = slot.as_mut() {
                if inj.cells.cancel.load(Ordering::Relaxed) || inj.pcm.is_empty() {
                    inj.cells.done.store(true, Ordering::Relaxed);
                    *slot = None;
                } else {
                    let mut buf: Vec<f32> = match inj.policy {
                        crate::AnnouncePolicy::Seize => Vec::with_capacity(samples.len()),
                        crate::AnnouncePolicy::MixUnder { .. } => samples.to_vec(),
                    };
                    let lin = match inj.policy {
                        crate::AnnouncePolicy::Seize => 1.0,
                        crate::AnnouncePolicy::MixUnder { gain_db } => 10_f32.powf(gain_db / 20.0),
                    };
                    for i in 0..samples.len() {
                        let a = inj.pcm.pop_front().map_or(0.0, |s| s * lin);
                        match inj.policy {
                            crate::AnnouncePolicy::Seize => buf.push(a),
                            crate::AnnouncePolicy::MixUnder { .. } => {
                                buf[i] = (buf[i] + a).clamp(-1.0, 1.0);
                            }
                        }
                    }
                    if inj.pcm.is_empty() {
                        inj.cells.done.store(true, Ordering::Relaxed);
                    }
                    work = std::borrow::Cow::Owned(buf);
                }
            }
        }
        self.dsp_quantize(&work, g);
        if let Some((tx, waker, _)) = dest {
            let mut sent = false;
            while self.buf_pcm.len() >= self.frame_samples {
                let frame: Vec<i16> = self.buf_pcm.drain(..self.frame_samples).collect();
                if tx.send(frame).is_ok() {
                    sent = true;
                }
            }
            if sent {
                let _ = waker.wake();
            }
        } else {
            // Not routed: keep at most a tail; never accumulate unbounded.
            if self.buf_pcm.len() >= self.frame_samples {
                self.buf_pcm.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::test_support_router;
    use crate::stream::test_support_router::NullBackend;

    #[test]
    fn frame_samples_is_20ms_at_any_rate() {
        assert_eq!(frame_samples(8_000), 160);
        assert_eq!(frame_samples(16_000), 320);
    }

    #[test]
    fn router_opens_one_mic_one_output_and_hands_back_call_audio() {
        let backend: Box<dyn crate::AudioBackend> = Box::new(NullBackend::new());
        let mut router = AudioRouter::new(backend);
        let mic = MicId::new("in:test");
        let out = OutputId::new("out:test");
        let audio = router
            .open_call(&mic, &out, crate::StreamConfig::default())
            .expect("router opens streams");
        // The call side gets a TX receiver and an RX sender.
        let _tx_rx: &std::sync::mpsc::Receiver<Vec<i16>> = &audio.tx_frames;
        let _rx_tx: &std::sync::mpsc::Sender<Vec<i16>> = &audio.rx_frames;
        assert_eq!(router.mic_count(), 1);
        assert_eq!(router.output_count(), 1);
    }

    #[test]
    fn open_mic_exposes_a_zeroed_capture_overrun_counter() {
        // iax-9e55: an open mic lane owns a capture-overrun cell, readable by
        // value and as a shared Arc. The NullBackend never drops buffers, so the
        // count stays 0; an unopened mic reports `None`.
        let backend: Box<dyn crate::AudioBackend> = Box::new(NullBackend::new());
        let mut router = AudioRouter::new(backend);
        let mic = MicId::new("in:test");
        assert_eq!(router.mic_capture_overruns(&mic), None);
        assert!(router.mic_overruns_cell(&mic).is_none());
        let _audio = router
            .open_call(
                &mic,
                &OutputId::new("out:test"),
                crate::StreamConfig::default(),
            )
            .expect("router opens streams");
        assert_eq!(router.mic_capture_overruns(&mic), Some(0));
        // The Arc and the by-value read observe the same cell.
        let cell = router.mic_overruns_cell(&mic).expect("cell after open");
        cell.fetch_add(3, Ordering::Relaxed);
        assert_eq!(router.mic_capture_overruns(&mic), Some(3));
    }

    #[test]
    fn captured_frames_flow_through_the_lane_to_call_audio() {
        // The N-lane router builds its own MicLane (DSP + framing). Drive the
        // opened lane via the NullBackend control surface and assert a 160-byte
        // frame appears on the call's TX channel.
        let (backend, controls) = test_support_router::NullBackend::with_controls();
        let mut router = AudioRouter::new(Box::new(backend));
        let audio = router
            .open_call(
                &MicId::new("in:test"),
                &OutputId::new("out:test"),
                crate::StreamConfig::default(),
            )
            .unwrap();
        controls.push_mic(&[0.5_f32; 160]);
        assert_eq!(audio.tx_frames.recv().unwrap().len(), 160);
    }

    #[test]
    fn two_calls_sharing_one_output_open_the_speaker_once() {
        let backend: Box<dyn crate::AudioBackend> =
            Box::new(test_support_router::MultiNullBackend::new());
        let mut router = AudioRouter::new(backend);
        let out = OutputId::new("out:shared");
        let _a = router
            .open_call(&MicId::new("in:a"), &out, crate::StreamConfig::default())
            .unwrap();
        let _b = router
            .open_call(&MicId::new("in:b"), &out, crate::StreamConfig::default())
            .unwrap();
        assert_eq!(router.mic_count(), 2, "two distinct mics");
        assert_eq!(router.output_count(), 1, "shared speaker opened once");
        assert_eq!(
            router.bus_call_count(&out),
            2,
            "both calls mix to the one bus"
        );
    }

    #[test]
    fn output_bus_reads_through_its_mixer() {
        use crate::mixer::Mixer;
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let mixer = Arc::new(Mutex::new(Mixer::new()));
        let (tx, rx) = channel();
        mixer.lock().unwrap().add_call(rx);
        let mut bus = OutputBus::new(Arc::clone(&mixer), 8_000);
        tx.send(vec![8000i16; 160]).unwrap();
        let mut out = [0.0_f32; 160];
        let n = crate::OutputSource::read(&mut bus, &mut out);
        assert_eq!(n, 160);
        assert!(out[0].abs() > 0.0);
    }

    #[test]
    fn output_bus_applies_rx_gain() {
        use crate::mixer::Mixer;
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let mixer = Arc::new(Mutex::new(Mixer::new()));
        let (tx, rx) = channel();
        mixer.lock().unwrap().add_call(rx);
        let mut bus = OutputBus::new(Arc::clone(&mixer), 8_000);
        let gain = bus.rx_gain();
        gain.store(0.5_f32.to_bits(), Ordering::Relaxed); // half volume
        tx.send(vec![16000i16; 160]).unwrap();
        let mut out = [0.0_f32; 160];
        let n = crate::OutputSource::read(&mut bus, &mut out);
        assert_eq!(n, 160);
        let full = f32::from(16000i16) / 32768.0;
        assert!(
            (out[0] - 0.5 * full).abs() < 1e-3,
            "rx gain halves the bus output: {}",
            out[0]
        );
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn output_bus_gain_4x_saturates_cleanly_no_wrap() {
        // iax-a4e7: raising the output gain ceiling to 4.0 must SATURATE
        // full-scale audio, never wrap. `OutputBus::read` applies gain in the
        // f32 domain and clamps to [-1, 1] before anything downstream
        // converts to i16, so a hot i16::MAX/i16::MIN frame at gain 4.0 must
        // land exactly at the rails with no sign flip.
        use crate::mixer::Mixer;
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let mixer = Arc::new(Mutex::new(Mixer::new()));
        let (tx, rx) = channel();
        mixer.lock().unwrap().add_call(rx);
        let mut bus = OutputBus::new(Arc::clone(&mixer), 8_000);
        let gain = bus.rx_gain();
        gain.store(4.0_f32.to_bits(), Ordering::Relaxed);
        // A full-scale frame alternating +max/-min so a wrap would flip sign.
        let frame: Vec<i16> = (0..160)
            .map(|i| if i % 2 == 0 { i16::MAX } else { i16::MIN })
            .collect();
        tx.send(frame).unwrap();
        let mut out = [0.0_f32; 160];
        let n = crate::OutputSource::read(&mut bus, &mut out);
        assert_eq!(n, 160);
        for (i, &s) in out[..n].iter().enumerate() {
            assert!(
                (-1.0..=1.0).contains(&s),
                "sample {i} out of [-1,1] at gain 4.0: {s}"
            );
            if i % 2 == 0 {
                assert!(
                    s > 0.0,
                    "even sample must stay positive (no sign flip): {s}"
                );
                assert!((s - 1.0).abs() < 1e-6, "even sample saturates at +1.0: {s}");
            } else {
                assert!(s < 0.0, "odd sample must stay negative (no sign flip): {s}");
                assert!((s + 1.0).abs() < 1e-6, "odd sample saturates at -1.0: {s}");
            }
        }
        // Mirror the downstream f32 -> i16 device conversion (stream.rs's
        // `fill_output` I16 path) to prove the FULL chain saturates end to
        // end, not just the bus's own clamp.
        let device: Vec<i16> = out[..n]
            .iter()
            .map(|s| (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)
            .collect();
        for (i, &d) in device.iter().enumerate() {
            if i % 2 == 0 {
                assert_eq!(d, i16::MAX, "even sample rails at i16::MAX, no wrap: {d}");
            } else {
                assert!(
                    d < 0,
                    "odd sample stays negative through i16 conversion, no wrap: {d}"
                );
            }
        }
    }

    // --- iax-a4e7 PHASE 1: RX/output compression (automatic RX leveling) ---

    #[test]
    fn output_bus_rx_compression_off_is_byte_identical_to_gain_only_path() {
        // Default OFF (byte-identical rule): with rx_compress untouched, the
        // bus must produce EXACTLY the same samples the gain-only stage would
        // (bit-for-bit) — the compressor branch must be a true no-op, not
        // merely "quiet", when disabled.
        use crate::mixer::Mixer;
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let mixer = Arc::new(Mutex::new(Mixer::new()));
        let (tx, rx) = channel();
        mixer.lock().unwrap().add_call(rx);
        let mut bus = OutputBus::new(Arc::clone(&mixer), 8_000);
        bus.rx_gain().store(1.5_f32.to_bits(), Ordering::Relaxed);
        let frame: Vec<i16> = (0..160).map(|i| (i * 97 % 4000) as i16 - 2000).collect();
        tx.send(frame.clone()).unwrap();
        let mut out = [0.0_f32; 160];
        let n = crate::OutputSource::read(&mut bus, &mut out);
        assert_eq!(n, 160);
        for (i, &s) in frame.iter().enumerate() {
            let expect = (f32::from(s) / 32768.0 * 1.5).clamp(-1.0, 1.0);
            assert_eq!(
                out[i].to_bits(),
                expect.to_bits(),
                "sample {i}: compression off must be an exact gain-only passthrough, got {} want {}",
                out[i],
                expect
            );
        }
    }

    #[test]
    #[allow(clippy::cast_precision_loss)] // n is a bounded 160-sample frame length
    fn output_bus_rx_compression_boosts_a_quiet_signal_toward_target() {
        // Leveling proof (iax-a4e7 PHASE 1): a steady, below-threshold signal
        // gets ONLY the compressor's makeup gain (no reduction, since it never
        // crosses the threshold) — so turning rx compression on must lift a
        // quiet station's RMS by (very close to) the makeup-gain ratio, not
        // just leave it alone. At the default strength (0.90): threshold
        // -26.4 dBFS, makeup 6.3 dB -> linear ratio 10^(6.3/20) ~= 2.065x.
        use crate::mixer::Mixer;
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};

        fn run(compress_on: bool) -> f32 {
            let mixer = Arc::new(Mutex::new(Mixer::new()));
            let (tx, rx) = channel();
            mixer.lock().unwrap().add_call(rx);
            let mut bus = OutputBus::new(Arc::clone(&mixer), 8_000);
            if compress_on {
                bus.rx_compress().store(true, Ordering::Relaxed);
            }
            // A constant-amplitude "quiet" frame: 400/32768 ~= -38.3 dBFS,
            // comfortably below the -26.4 dBFS default threshold.
            tx.send(vec![400i16; 160]).unwrap();
            let mut out = [0.0_f32; 160];
            let n = crate::OutputSource::read(&mut bus, &mut out);
            assert_eq!(n, 160);
            // RMS of a constant-magnitude frame is just its magnitude.
            out[..n].iter().map(|s| s.abs()).sum::<f32>() / n as f32
        }

        let off = run(false);
        let on = run(true);
        let expected_ratio = 10f32.powf(6.3 / 20.0); // ~= 2.065
        let ratio = on / off;
        assert!(
            ratio > 1.5,
            "rx compression must materially boost a quiet signal's RMS: off={off} on={on} ratio={ratio}"
        );
        assert!(
            (ratio - expected_ratio).abs() < 0.05,
            "boost should match the makeup-gain ratio ~{expected_ratio}: got {ratio} (off={off} on={on})"
        );
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn output_bus_rx_compression_plus_gain_4x_stays_railed_clean_no_wrap() {
        // Saturation preserved: with RX compression ON *and* the bus gain at
        // its 4x ceiling (iax-a4e7), a full-scale alternating +max/-min frame
        // must still clamp cleanly to [-1, 1] downstream — no wrap, no sign
        // flip — exactly like `output_bus_gain_4x_saturates_cleanly_no_wrap`,
        // proving the two stages compose safely.
        use crate::mixer::Mixer;
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let mixer = Arc::new(Mutex::new(Mixer::new()));
        let (tx, rx) = channel();
        mixer.lock().unwrap().add_call(rx);
        let mut bus = OutputBus::new(Arc::clone(&mixer), 8_000);
        bus.rx_compress().store(true, Ordering::Relaxed);
        bus.rx_gain().store(4.0_f32.to_bits(), Ordering::Relaxed);
        let frame: Vec<i16> = (0..160)
            .map(|i| if i % 2 == 0 { i16::MAX } else { i16::MIN })
            .collect();
        tx.send(frame).unwrap();
        let mut out = [0.0_f32; 160];
        let n = crate::OutputSource::read(&mut bus, &mut out);
        assert_eq!(n, 160);
        for (i, &s) in out[..n].iter().enumerate() {
            assert!(
                (-1.0..=1.0).contains(&s),
                "sample {i} out of [-1,1] with compression+4x gain: {s}"
            );
        }
        let device: Vec<i16> = out[..n]
            .iter()
            .map(|s| (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)
            .collect();
        for (i, &d) in device.iter().enumerate() {
            if i % 2 == 0 {
                assert!(d >= 0, "even sample must stay non-negative, no wrap: {d}");
            } else {
                assert!(d <= 0, "odd sample must stay non-positive, no wrap: {d}");
            }
        }
    }

    #[test]
    fn router_set_output_compress_level_clamps_to_0_1() {
        // Mirrors `router_set_output_gain_clamps_to_0_4`: the router enforces
        // the same [0.0, 1.0] ceiling as ConsoleSession/Station.
        let backend: Box<dyn crate::AudioBackend> = Box::new(NullBackend::new());
        let mut router = AudioRouter::new(backend);
        let mic = MicId::new("in:test");
        let out = OutputId::new("out:test");
        let _audio = router
            .open_call(&mic, &out, crate::StreamConfig::default())
            .unwrap();
        assert_eq!(
            router.output_compress(&out),
            Some(false),
            "compression defaults OFF"
        );
        router.set_output_compress(&out, true);
        assert_eq!(router.output_compress(&out), Some(true));
        router.set_output_compress_level(&out, 5.0); // way over
        assert_eq!(router.output_compress_level(&out), Some(1.0));
        router.set_output_compress_level(&out, -1.0);
        assert_eq!(router.output_compress_level(&out), Some(0.0));
    }

    // Q1 accepted: dest payload is (Sender, Arc<mio::Waker>, lead cell). A test
    // waker over a throwaway mio::Poll registry; `wake()` is a no-op observable
    // side effect here. The lead cell (iax-2733) is a fresh throwaway.
    fn test_dest(tx: std::sync::mpsc::Sender<Vec<i16>>) -> MicDestUnit {
        test_dest_with_lead(tx).0
    }

    /// Like `test_dest` but also returns the pre-roll lead cell so a test can
    /// assert how many frames the lane flushed AHEAD of the live stream on a
    /// key-up edge (iax-2733).
    fn test_dest_with_lead(
        tx: std::sync::mpsc::Sender<Vec<i16>>,
    ) -> (MicDestUnit, std::sync::Arc<AtomicU32>) {
        let poll = mio::Poll::new().unwrap();
        let waker = std::sync::Arc::new(mio::Waker::new(poll.registry(), mio::Token(0)).unwrap());
        let lead = std::sync::Arc::new(AtomicU32::new(0));
        ((tx, waker, std::sync::Arc::clone(&lead)), lead)
    }

    #[test]
    fn mic_lane_emits_160_byte_frames_only_while_keyed_to_current_dest() {
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let dest = Arc::new(Mutex::new(None));
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut lane = MicLane::new(
            8_000,
            Arc::clone(&dest),
            Arc::clone(&gate),
            Arc::new(Mutex::new(None)),
            Arc::new(std::sync::atomic::AtomicU32::new(0)),
            Arc::new(Mutex::new(None)),
        );

        // No dest set + not keyed → nothing emitted.
        crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);

        // Wire a destination (Sender + Waker) + key up → one 160-byte frame.
        let (tx, rx) = channel();
        *dest.lock().unwrap() = Some(test_dest(tx));
        gate.store(true, std::sync::atomic::Ordering::Relaxed);
        crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);
        assert_eq!(rx.recv().unwrap().len(), 160);
    }

    #[test]
    fn mic_lane_swapping_dest_redirects_frames_and_waker() {
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let dest = Arc::new(Mutex::new(None));
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut lane = MicLane::new(
            8_000,
            Arc::clone(&dest),
            Arc::clone(&gate),
            Arc::new(Mutex::new(None)),
            Arc::new(std::sync::atomic::AtomicU32::new(0)),
            Arc::new(Mutex::new(None)),
        );
        let (tx_x, rx_x) = channel();
        let (tx_y, rx_y) = channel();
        // Sender and Waker swap together as one unit (Q1).
        *dest.lock().unwrap() = Some(test_dest(tx_x));
        crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);
        assert_eq!(rx_x.recv().unwrap().len(), 160);
        // Swap to Y — frames (and the woken loop) now go to Y, not X.
        *dest.lock().unwrap() = Some(test_dest(tx_y));
        crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);
        assert_eq!(rx_y.recv().unwrap().len(), 160);
        assert!(rx_x.try_recv().is_err(), "old dest no longer receives");
    }

    // --- iax-750a: TX trim (always-on final gain stage after the compressor) ---

    /// Build a keyed lane feeding a throwaway dest, run one 160-sample frame of
    /// constant `input` through it (compression per `compress`, trim per `trim`
    /// — `None` leaves the cell at its default), and return the post-DSP TX
    /// peak plus the emitted PCM frame.
    fn run_lane_frame(input: f32, compress: bool, trim: Option<f32>) -> (f32, Vec<i16>) {
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let dest = Arc::new(Mutex::new(None));
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut lane = MicLane::new(
            8_000,
            Arc::clone(&dest),
            Arc::clone(&gate),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(Mutex::new(None)),
        );
        lane.compress_flag().store(compress, Ordering::Relaxed);
        if let Some(t) = trim {
            lane.tx_trim_cell().store(t.to_bits(), Ordering::Relaxed);
        }
        let (tx, rx) = channel();
        *dest.lock().unwrap() = Some(test_dest(tx));
        crate::InputSink::write(&mut lane, &[input; 160], input);
        let peak = f32::from_bits(lane.tx_peak().load(Ordering::Relaxed));
        (peak, rx.recv().expect("keyed lane emits one frame"))
    }

    #[test]
    fn mic_lane_tx_trim_halves_post_compressor_output() {
        // With compression ON, a 0.5 trim halves what the compressor produced —
        // trim is the FINAL stage, after makeup gain, and the TX meter sees it.
        let (unity_peak, _) = run_lane_frame(0.5, true, Some(1.0));
        let (halved_peak, _) = run_lane_frame(0.5, true, Some(0.5));
        assert!(unity_peak > 0.0, "compressor output is nonzero");
        assert!(
            (halved_peak - 0.5 * unity_peak).abs() < 1e-6,
            "0.5 trim halves the post-compressor peak: unity={unity_peak} halved={halved_peak}"
        );
    }

    #[test]
    fn mic_lane_unity_tx_trim_is_bit_exact_passthrough() {
        // Unity trim must be indistinguishable from the stage not existing:
        // an untouched lane (default 1.0) and an explicit 1.0 emit the same
        // PCM samples, with and without compression.
        for compress in [false, true] {
            let (default_peak, default_frame) = run_lane_frame(0.5, compress, None);
            let (unity_peak, unity_frame) = run_lane_frame(0.5, compress, Some(1.0));
            assert_eq!(
                default_frame, unity_frame,
                "unity trim is bit-exact (compress={compress})"
            );
            assert_eq!(
                default_peak.to_bits(),
                unity_peak.to_bits(),
                "unity trim leaves the TX peak bit-exact (compress={compress})"
            );
        }
    }

    #[test]
    fn mic_lane_tx_trim_applies_with_compression_off() {
        // Trim is ALWAYS on — not scoped to the compressor. With compression
        // off, a 0.5 trim on a 0.5 input yields exactly a 0.25 peak.
        let (peak, _) = run_lane_frame(0.5, false, Some(0.5));
        assert!(
            (peak - 0.25).abs() < 1e-6,
            "0.5 trim halves the uncompressed 0.5 input: {peak}"
        );
    }

    #[test]
    fn mic_lane_tx_trim_boost_clamps_at_full_scale() {
        // Trim 2.0 boosts; a sample that would exceed full scale (0.8 * 2.0 =
        // 1.6) clamps to EXACTLY 1.0, like the input-gain stage.
        let (peak, frame) = run_lane_frame(0.8, false, Some(2.0));
        assert_eq!(
            peak.to_bits(),
            1.0_f32.to_bits(),
            "boosted peak clamps to exactly 1.0: {peak}"
        );
        // The emitted PCM is full scale too, not wrapped/overflowed.
        assert_eq!(frame[0], 32767, "PCM sample is full scale");
        // A boost that stays in range is NOT clamped: 0.3 * 2.0 = 0.6.
        let (peak, _) = run_lane_frame(0.3, false, Some(2.0));
        assert!(
            (peak - 0.6).abs() < 1e-6,
            "in-range boost is a plain doubling: {peak}"
        );
    }

    #[test]
    fn router_set_mic_tx_trim_clamps_out_of_range_input() {
        // The router setter clamps to 0.0..=2.0. Observed through the lane's
        // post-DSP TX meter (no getter — YAGNI): 5.0 clamps to 2.0 (a 0.4 input
        // meters at 0.8, NOT full scale), and -1.0 clamps to 0.0 (silence).
        let (backend, controls) = test_support_router::NullBackend::with_controls();
        let mut router = AudioRouter::new(Box::new(backend));
        let mic = MicId::new("in:test");
        let out = OutputId::new("out:test");
        // open_call keys the lane, so pushed mic audio runs dsp_quantize.
        let _audio = router
            .open_call(&mic, &out, crate::StreamConfig::default())
            .unwrap();
        router.set_mic_tx_trim(&mic, 5.0); // → 2.0
        controls.push_mic(&[0.4_f32; 160]);
        let dbfs = router.mic_tx_dbfs(&mic).expect("mic open");
        let expect = 20.0 * 0.8_f32.log10(); // 0.4 * 2.0
        assert!(
            (dbfs - expect).abs() < 0.05,
            "5.0 clamps to 2.0 (peak 0.8 = {expect} dBFS): {dbfs}"
        );
        router.set_mic_tx_trim(&mic, -1.0); // → 0.0
        controls.push_mic(&[0.4_f32; 160]);
        let dbfs = router.mic_tx_dbfs(&mic).expect("mic open");
        assert!(
            (dbfs + 60.0).abs() < 1e-3,
            "-1.0 clamps to 0.0: TX meter floors at -60: {dbfs}"
        );
    }

    #[test]
    fn router_set_output_gain_clamps_to_0_4() {
        // iax-a4e7: the router enforces the same [0.0, 4.0] ceiling as
        // ConsoleSession/Station — defense-in-depth for any caller that
        // reaches the router directly. 0.0 must still mute: the half-duplex
        // RX-mute path calls set_output_gain(0) and depends on it working.
        let backend: Box<dyn crate::AudioBackend> = Box::new(NullBackend::new());
        let mut router = AudioRouter::new(backend);
        let mic = MicId::new("in:test");
        let out = OutputId::new("out:test");
        let _audio = router
            .open_call(&mic, &out, crate::StreamConfig::default())
            .unwrap();
        router.set_output_gain(&out, 5.0); // way over the new cap
        assert_eq!(router.output_gain(&out), Some(4.0), "5.0 clamps to 4.0");
        router.set_output_gain(&out, -1.0);
        assert_eq!(
            router.output_gain(&out),
            Some(0.0),
            "-1.0 clamps to 0.0 (mute)"
        );
        router.set_output_gain(&out, 0.0);
        assert_eq!(
            router.output_gain(&out),
            Some(0.0),
            "0.0 is a valid floor, still mutes"
        );
    }

    // --- iax-2733: VOX pre-roll / look-back buffer ---

    /// A PCM frame is silent iff every sample is 0.
    fn is_silent(frame: &[i16]) -> bool {
        frame.iter().all(|&s| s == 0)
    }

    #[test]
    fn preroll_disabled_keeps_cheap_unkeyed_early_return() {
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let dest = Arc::new(Mutex::new(None));
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut lane = MicLane::new(
            8_000,
            Arc::clone(&dest),
            Arc::clone(&gate),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(Mutex::new(None)),
        );
        // preroll_ms defaults to 0 (disabled). Push several UNKEYED non-silent
        // frames; with pre-roll off the lane must NOT buffer them (today's cheap
        // early-return) — nothing is retained.
        let (tx, rx) = channel();
        let (unit, lead) = test_dest_with_lead(tx);
        *dest.lock().unwrap() = Some(unit);
        for _ in 0..5 {
            crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);
        }
        // Key up and push ONE live frame.
        gate.store(true, std::sync::atomic::Ordering::Relaxed);
        crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);
        // Exactly one (live) frame, zero pre-roll lead: no look-back was kept.
        let frames: Vec<Vec<i16>> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(frames.len(), 1, "pre-roll disabled must not prepend frames");
        assert_eq!(lead.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn preroll_buffers_unkeyed_frames_and_flushes_them_ahead_on_keyup() {
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let dest = Arc::new(Mutex::new(None));
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut lane = MicLane::new(
            8_000,
            Arc::clone(&dest),
            Arc::clone(&gate),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(Mutex::new(None)),
        );
        // 60 ms of pre-roll = 3 frames.
        lane.preroll_ms_cell()
            .store(60, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = channel();
        let (unit, lead) = test_dest_with_lead(tx);
        *dest.lock().unwrap() = Some(unit);
        // Push 3 UNKEYED non-silent frames — the speech onset that today would be
        // clipped. They must be retained, NOT sent.
        for _ in 0..3 {
            crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);
        }
        assert!(
            rx.try_recv().is_err(),
            "unkeyed pre-roll frames must not reach the dest yet"
        );
        // Key up + push one live frame: the 3 buffered frames flush AHEAD of the
        // live frame, and the lane signals a 3-frame lead to the run-loop.
        gate.store(true, std::sync::atomic::Ordering::Relaxed);
        crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);
        let frames: Vec<Vec<i16>> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(frames.len(), 4, "3 pre-roll + 1 live frame");
        assert_eq!(
            lead.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "lane must signal a 3-frame pre-roll lead"
        );
        // The flushed pre-roll carries the buffered (non-silent) onset.
        assert!(
            frames.iter().all(|f| f.len() == 160 && !is_silent(f)),
            "flushed frames must be the buffered speech onset"
        );
    }

    #[test]
    fn preroll_ring_caps_at_the_configured_length() {
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let dest = Arc::new(Mutex::new(None));
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut lane = MicLane::new(
            8_000,
            Arc::clone(&dest),
            Arc::clone(&gate),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(Mutex::new(None)),
        );
        // 40 ms = 2 frames of pre-roll, but push 6 unkeyed frames: only the
        // newest 2 may be retained (bounded ring).
        lane.preroll_ms_cell()
            .store(40, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = channel();
        let (unit, lead) = test_dest_with_lead(tx);
        *dest.lock().unwrap() = Some(unit);
        for _ in 0..6 {
            crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);
        }
        gate.store(true, std::sync::atomic::Ordering::Relaxed);
        crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);
        let frames: Vec<Vec<i16>> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(frames.len(), 3, "2 capped pre-roll + 1 live frame");
        assert_eq!(lead.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn preroll_resets_lead_and_ring_after_unkey() {
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let dest = Arc::new(Mutex::new(None));
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut lane = MicLane::new(
            8_000,
            Arc::clone(&dest),
            Arc::clone(&gate),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(Mutex::new(None)),
        );
        lane.preroll_ms_cell()
            .store(40, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = channel();
        let (unit, _lead) = test_dest_with_lead(tx);
        *dest.lock().unwrap() = Some(unit);
        // First over: buffer 2, key, flush.
        for _ in 0..2 {
            crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);
        }
        gate.store(true, std::sync::atomic::Ordering::Relaxed);
        crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);
        while rx.try_recv().is_ok() {}
        // Unkey (no fresh pre-roll captured this gap), then key again with an
        // EMPTY ring → only the live frame flows, no stale pre-roll prepended.
        gate.store(false, std::sync::atomic::Ordering::Relaxed);
        crate::InputSink::write(&mut lane, &[0.0_f32; 160], 0.0); // unkeyed: re-buffers 1
        gate.store(true, std::sync::atomic::Ordering::Relaxed);
        crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5);
        let frames: Vec<Vec<i16>> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        // One re-buffered unkeyed frame (the single silent frame) + the live one.
        assert_eq!(frames.len(), 2, "fresh over flushes only its own look-back");
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn mic_lane_applies_calibrated_notch_when_profile_set() {
        use crate::{MicProfile, NotchSpec};
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let profile = MicProfile {
            highpass_hz: 90.0,
            notches: vec![NotchSpec {
                freq_hz: 450.0,
                q: 20.0,
            }],
            noise_floor_dbfs: -50.0,
            gate_threshold_db: -44.0,
        };
        let dest = Arc::new(Mutex::new(None));
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let profile_cell = Arc::new(Mutex::new(None));
        let profile_gen = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut lane = MicLane::new(
            8_000,
            Arc::clone(&dest),
            Arc::clone(&gate),
            Arc::clone(&profile_cell),
            Arc::clone(&profile_gen),
            Arc::new(Mutex::new(None)),
        );
        lane.denoise_flag()
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Push the profile via the shared cell + generation bump (the router
        // path): the lane rebuilds its NoiseReducer on the next write().
        *profile_cell.lock().unwrap() = Some(profile.clone());
        profile_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = channel();
        *dest.lock().unwrap() = Some(test_dest(tx));
        // Drive a 450 Hz tone through several frames; the calibrated notch must
        // attenuate it. Normalize the emitted PCM frames and check the tail peak.
        let tone: Vec<f32> = (0..4000)
            .map(|i| (std::f32::consts::TAU * 450.0 * i as f32 / 8000.0).sin())
            .collect();
        crate::InputSink::write(&mut lane, &tone, 0.5);
        // Normalize the emitted PCM frames in order, then check the tail (the
        // notch needs ~a few hundred samples to settle — the proven
        // denoise::from_profile_notches_the_measured_tone test measures the
        // second half for the same reason).
        let mut decoded: Vec<f32> = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            decoded.extend(frame.into_iter().map(|s| f32::from(s) / 32768.0));
        }
        let tail_peak = decoded[decoded.len() / 2..]
            .iter()
            .fold(0.0_f32, |p, &s| p.max(s.abs()));
        assert!(
            tail_peak < 0.5,
            "calibrated 450 Hz notch should attenuate the tone: {tail_peak}"
        );
    }

    #[test]
    fn router_meters_mic_input_continuously_while_unkeyed() {
        // iax-5c30: the mic INPUT level must be metered continuously, even when
        // the PTT gate is closed (unkeyed), so a consumer's VOX can key from
        // silence. The dedicated `mic_input_dbfs` rises above the floor while
        // unkeyed; `mic_tx_dbfs` (post-DSP, keyed-only) stays floored.
        let (backend, controls) = test_support_router::NullBackend::with_controls();
        let mut router = AudioRouter::new(Box::new(backend));
        let mic = MicId::new("in:test");
        let out = OutputId::new("out:test");
        let _audio = router
            .open_call(&mic, &out, crate::StreamConfig::default())
            .unwrap();
        // Unkey the lane, then push a loud mic frame.
        router.set_gate(&mic, false);
        controls.push_mic(&[0.5_f32; 160]);
        let input = router.mic_input_dbfs(&mic).expect("mic open");
        let tx = router.mic_tx_dbfs(&mic).expect("mic open");
        assert!(
            input > -60.0,
            "unkeyed input level should rise above the floor: {input}"
        );
        assert!(
            (tx + 60.0).abs() < 1e-3,
            "unkeyed TX level stays floored at -60: {tx}"
        );
    }

    #[test]
    fn router_input_metering_tracks_when_keyed() {
        // KEYED: both the dedicated input meter and the TX meter track.
        let (backend, controls) = test_support_router::NullBackend::with_controls();
        let mut router = AudioRouter::new(Box::new(backend));
        let mic = MicId::new("in:test");
        let out = OutputId::new("out:test");
        let _audio = router
            .open_call(&mic, &out, crate::StreamConfig::default())
            .unwrap();
        // open_call keys the lane; push a loud frame.
        controls.push_mic(&[0.5_f32; 160]);
        let input = router.mic_input_dbfs(&mic).expect("mic open");
        let tx = router.mic_tx_dbfs(&mic).expect("mic open");
        assert!(input > -60.0, "keyed input level tracks: {input}");
        assert!(tx > -60.0, "keyed TX level tracks: {tx}");
    }

    #[test]
    fn router_input_metering_reflects_input_gain() {
        // The input-gain slider must affect the metered input level (VOX
        // sensitivity rides the gain): tap point is POST-gain.
        let (backend, controls) = test_support_router::NullBackend::with_controls();
        let mut router = AudioRouter::new(Box::new(backend));
        let mic = MicId::new("in:test");
        let out = OutputId::new("out:test");
        let _audio = router
            .open_call(&mic, &out, crate::StreamConfig::default())
            .unwrap();
        router.set_gate(&mic, false);
        router.set_mic_gain(&mic, 0.1); // attenuate
        controls.push_mic(&[0.5_f32; 160]);
        let quiet = router.mic_input_dbfs(&mic).unwrap();
        router.set_mic_gain(&mic, 2.0); // boost
        controls.push_mic(&[0.5_f32; 160]);
        let loud = router.mic_input_dbfs(&mic).unwrap();
        assert!(
            loud > quiet + 6.0,
            "higher input gain raises the metered input level: quiet={quiet} loud={loud}"
        );
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn router_tx_spectrum_reflects_a_driven_tone() {
        // iax-2b09: the live-call TX spectrum taps the post-DSP, pre-encode PCM
        // on the mic lane — a 1 kHz tone pushed through the keyed lane must light
        // its log bin, using the SAME analyzer the mic monitor uses.
        use crate::spectrum::{SPECTRUM_BINS, SpectrumAnalyzer};
        use std::f32::consts::TAU;
        let (backend, controls) = test_support_router::NullBackend::with_controls();
        let mut router = AudioRouter::new(Box::new(backend));
        let mic = MicId::new("in:test");
        let out = OutputId::new("out:test");
        // open_call keys the lane; the dest is bound so dsp_quantize runs.
        let _audio = router
            .open_call(&mic, &out, crate::StreamConfig::default())
            .unwrap();
        let tone: Vec<f32> = (0..(2_048 * 4))
            .map(|i| 0.8 * (TAU * 1_000.0 * i as f32 / 8_000.0).sin())
            .collect();
        controls.push_mic(&tone);
        let mut bins = [0.0_f32; SPECTRUM_BINS];
        let n = router
            .mic_tx_spectrum(&mic, &mut bins)
            .expect("mic lane open");
        assert_eq!(n, SPECTRUM_BINS);
        let bin = SpectrumAnalyzer::new(8_000).bin_of_hz(1_000.0);
        assert!(
            bins[bin] > -30.0,
            "1 kHz TX tone should light its log bin: {}",
            bins[bin]
        );
    }

    #[test]
    fn router_tx_spectrum_is_none_for_unopened_mic() {
        // iax-2b09: an unopened mic has no lane, hence no spectrum.
        let router = AudioRouter::new(Box::new(NullBackend::new()));
        let mut bins = [0.0_f32; crate::spectrum::SPECTRUM_BINS];
        assert!(
            router
                .mic_tx_spectrum(&MicId::new("nope"), &mut bins)
                .is_none()
        );
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn router_rx_spectrum_reflects_decoded_audio() {
        // iax-2b09: the live-call RX spectrum taps the post-mix decoded PCM on
        // the output bus. Drive a steady PCM level through the bus mixer and
        // confirm the spectrum rises off the floor.
        use crate::mixer::Mixer;
        use crate::spectrum::SPECTRUM_BINS;
        use std::f32::consts::TAU;
        use std::sync::mpsc::channel;
        let mixer = Arc::new(Mutex::new(Mixer::new()));
        let (tx, rx) = channel();
        mixer.lock().unwrap().add_call(rx);
        let bus = OutputBus::new(Arc::clone(&mixer), 8_000);
        let rx_spectrum = bus.rx_spectrum();
        let mut bus = bus;
        // Feed several full FFT windows worth of a 1 kHz tone as PCM frames.
        for _ in 0..=((2_048 * 4) / 160) {
            #[allow(clippy::cast_possible_truncation)]
            let frame: Vec<i16> = (0..160)
                .map(|i| (0.8 * 32_767.0 * (TAU * 1_000.0 * i as f32 / 8_000.0).sin()) as i16)
                .collect();
            tx.send(frame).unwrap();
            let mut out = [0.0_f32; 160];
            let _ = crate::OutputSource::read(&mut bus, &mut out);
        }
        let mut bins = [0.0_f32; SPECTRUM_BINS];
        let n = rx_spectrum.lock().unwrap().copy_into(&mut bins);
        assert_eq!(n, SPECTRUM_BINS);
        let loudest = bins.iter().copied().fold(f32::MIN, f32::max);
        assert!(
            loudest > -30.0,
            "decoded RX tone should light the spectrum off the floor: {loudest}"
        );
    }

    #[test]
    fn router_rx_spectrum_is_none_for_unopened_bus() {
        // iax-2b09: an unopened bus has no spectrum.
        let router = AudioRouter::new(Box::new(NullBackend::new()));
        let mut bins = [0.0_f32; crate::spectrum::SPECTRUM_BINS];
        assert!(
            router
                .output_rx_spectrum(&OutputId::new("nope"), &mut bins)
                .is_none()
        );
    }

    #[test]
    fn router_clamps_vox_preroll_ms_to_the_max() {
        // iax-2733: the control side clamps pre-roll to [0, MAX_PREROLL_MS]; the
        // ring is sized to the max, so an out-of-range request saturates.
        let backend: Box<dyn crate::AudioBackend> = Box::new(NullBackend::new());
        let mut router = AudioRouter::new(backend);
        let mic = MicId::new("in:test");
        let out = OutputId::new("out:test");
        let _audio = router
            .open_call(&mic, &out, crate::StreamConfig::default())
            .unwrap();
        router.set_mic_preroll_ms(&mic, 1_000); // way over the cap
        assert_eq!(router.mic_preroll_ms(&mic), Some(MAX_PREROLL_MS));
        router.set_mic_preroll_ms(&mic, 100);
        assert_eq!(router.mic_preroll_ms(&mic), Some(100));
        router.set_mic_preroll_ms(&mic, 0); // disabled
        assert_eq!(router.mic_preroll_ms(&mic), Some(0));
    }

    #[test]
    fn mic_lane_drops_partial_tail_on_unkey() {
        // Mirrors audio_bridge::mic_sink_drops_partial_tail_on_unkey.
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex};
        let dest = Arc::new(Mutex::new(None));
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut lane = MicLane::new(
            8_000,
            Arc::clone(&dest),
            Arc::clone(&gate),
            Arc::new(Mutex::new(None)),
            Arc::new(std::sync::atomic::AtomicU32::new(0)),
            Arc::new(Mutex::new(None)),
        );
        let (tx, rx) = channel();
        *dest.lock().unwrap() = Some(test_dest(tx));
        crate::InputSink::write(&mut lane, &[0.5_f32; 100], 0.5); // partial
        gate.store(false, std::sync::atomic::Ordering::Relaxed);
        crate::InputSink::write(&mut lane, &[0.5_f32; 100], 0.5); // unkey clears tail
        gate.store(true, std::sync::atomic::Ordering::Relaxed);
        crate::InputSink::write(&mut lane, &[0.5_f32; 160], 0.5); // fresh key
        assert_eq!(rx.recv().unwrap().len(), 160);
        assert!(rx.try_recv().is_err(), "stale tail must be gone");
    }

    #[test]
    fn seize_inject_replaces_mic_audio_on_tx() {
        // A keyed mic lane that is fed silence by the capture callback, but has a
        // Seize announcement queued, emits the ANNOUNCEMENT on TX, not the mic.
        let (backend, controls) = test_support_router::NullBackend::with_controls();
        let mut router = AudioRouter::new(Box::new(backend));
        let mic = MicId::new("in:test");
        let audio = router
            .open_call(
                &mic,
                &OutputId::new("out:test"),
                crate::StreamConfig::default(),
            )
            .unwrap();
        // 320 samples of full-scale tone as the announcement (2 frames).
        let pcm: std::sync::Arc<[i16]> = (0..320).map(|_| 20_000_i16).collect::<Vec<_>>().into();
        let handle = router
            .play_into_mic(
                &mic,
                std::sync::Arc::clone(&pcm),
                crate::AnnouncePolicy::Seize,
            )
            .expect("mic is open");
        // Drive two capture callbacks of SILENCE; the lane should emit the tone.
        controls.push_mic(&[0.0_f32; 160]);
        controls.push_mic(&[0.0_f32; 160]);
        let frame = audio.tx_frames.recv().unwrap();
        assert_eq!(frame.len(), 160);
        // The first PCM sample has a large magnitude — it's the tone, not the
        // silence the mic captured.
        let decoded = frame[0];
        assert!(
            decoded.abs() > 8_000,
            "announcement audio on TX, got {decoded}"
        );
        // After both frames drain, the handle reports done.
        let _ = audio.tx_frames.recv().unwrap();
        controls.push_mic(&[0.0_f32; 160]);
        assert!(handle.is_done(), "handle done once PCM exhausted");
    }

    #[test]
    fn mixunder_sums_announcement_over_live_mic() {
        let (backend, controls) = test_support_router::NullBackend::with_controls();
        let mut router = AudioRouter::new(Box::new(backend));
        let mic = MicId::new("in:test");
        let audio = router
            .open_call(
                &mic,
                &OutputId::new("out:test"),
                crate::StreamConfig::default(),
            )
            .unwrap();
        // Announcement at -6 dB (~0.5 linear) of half-scale → ~0.25 added.
        let pcm: std::sync::Arc<[i16]> = (0..160).map(|_| 16_384_i16).collect::<Vec<_>>().into();
        let h = router
            .play_into_mic(&mic, pcm, crate::AnnouncePolicy::MixUnder { gain_db: -6.0 })
            .unwrap();
        // Live mic carries a steady 0.25 level.
        controls.push_mic(&[0.25_f32; 160]);
        let frame = audio.tx_frames.recv().unwrap();
        let summed = f32::from(frame[0]) / 32768.0;
        // Sum is louder than the live-only 0.25 (announcement added on top).
        assert!(summed > 0.30, "mixunder adds to live audio, got {summed}");
        assert!(
            h.is_done(),
            "single-frame announcement marks the handle done once consumed"
        );
    }

    #[test]
    fn play_into_bus_mixes_a_finite_announcement() {
        let backend: Box<dyn crate::AudioBackend> =
            Box::new(test_support_router::NullBackend::new());
        let mut router = AudioRouter::new(backend);
        let out = OutputId::new("out:test");
        // Open the bus via a monitor call.
        let _c = router
            .open_monitor_call(&out, crate::StreamConfig::default())
            .unwrap();
        let pcm: std::sync::Arc<[i16]> = vec![16_000_i16; 160].into();
        let h = router.play_into_bus(&out, pcm, 8_000).expect("bus open");
        assert!(!h.is_done());
        assert_eq!(
            router.bus_call_count(&out),
            2,
            "monitor lane + announcement lane"
        );
    }

    #[test]
    fn cancel_stops_injection_early() {
        let (backend, controls) = test_support_router::NullBackend::with_controls();
        let mut router = AudioRouter::new(Box::new(backend));
        let mic = MicId::new("in:test");
        let _audio = router
            .open_call(
                &mic,
                &OutputId::new("out:test"),
                crate::StreamConfig::default(),
            )
            .unwrap();
        let pcm: std::sync::Arc<[i16]> = vec![10_000_i16; 1600].into(); // 10 frames
        let h = router
            .play_into_mic(&mic, pcm, crate::AnnouncePolicy::Seize)
            .unwrap();
        h.cancel();
        controls.push_mic(&[0.0_f32; 160]); // first callback observes cancel
        assert!(h.is_done(), "cancel marks done on next callback");
    }
}
