// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A third IAX2 peer that links into astar-server's listener.
//!
//! Built on `astar_iax::dial_raw_with_profile` (the `test-support` raw dial):
//! no audio devices, the call's PCM frame channels exposed directly. It
//! presents itself the way an `app_rpt` node does on a link —
//! `CALLING_NUMBER` = its own node number (1997 by default),
//! `CALLED_NUMBER` = the target node — so astar-server's inbound allowlist and
//! roster see a node, not a guest.
//!
//! A drain thread takes every received frame and call event off the call as
//! it arrives and timestamps it, so nothing is lost between scenario steps:
//! [`IaxProbe::record`] (or [`IaxProbe::mark`] + [`IaxProbe::take_since`])
//! slices that log.
//!
//! Target address is always explicit (`127.0.0.1:<port>`); nothing here
//! resolves a node number through `AllStar` DNS.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use astar_iax::{CallEvent, CallProfile, CodecPolicy, IaxError, RawDial, TxFrames};
use astar_iax_core::subclass::VoiceFormat;

use super::{RxFrame, TxReport, frames_of, sleep_until};
use crate::audio;

/// How to dial.
#[derive(Debug, Clone)]
pub struct IaxProbeConfig {
    /// astar-server's IAX2 listener, e.g. `127.0.0.1:4569`.
    pub server: SocketAddr,
    /// Our node number: sent as `CALLING_NUMBER` and as the IAX2 username.
    pub node: String,
    /// The node we call (`CALLED_NUMBER`), e.g. astar-server's `1998`.
    pub dest: String,
    /// MD5 secret if the server demands one; empty for `auth = off`. A dial
    /// argument only, never stored on the probe.
    pub secret: String,
    /// What we advertise; also fixes our PCM rate (8 kHz, or 16 kHz for
    /// `PreferSlin16`).
    pub codec_policy: CodecPolicy,
    /// How long to wait for the call to be answered.
    pub answer_timeout: Duration,
}

impl IaxProbeConfig {
    /// Node 1997 calling `dest` at `server`, µ-law, no secret, 10 s to answer.
    #[must_use]
    pub fn new(server: SocketAddr, dest: impl Into<String>) -> Self {
        Self {
            server,
            node: "1997".into(),
            dest: dest.into(),
            secret: String::new(),
            codec_policy: CodecPolicy::UlawOnly,
            answer_timeout: Duration::from_secs(10),
        }
    }
}

/// Why a probe could not get a call up.
#[derive(Debug)]
pub enum ProbeError {
    /// The runtime could not start.
    Iax(IaxError),
    /// The call ended before it was answered (reason from the engine).
    Hungup(String),
    /// Not answered within `answer_timeout`.
    Timeout,
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Iax(e) => write!(f, "iax: {e}"),
            Self::Hungup(r) => write!(f, "hung up before answer: {r}"),
            Self::Timeout => f.write_str("not answered in time"),
        }
    }
}

impl std::error::Error for ProbeError {}

impl From<IaxError> for ProbeError {
    fn from(e: IaxError) -> Self {
        Self::Iax(e)
    }
}

/// One call event as the drain thread saw it.
#[derive(Debug, Clone)]
pub struct EventRecord {
    pub at: Instant,
    pub event: ProbeEvent,
}

/// The call events the bench cares about, flattened to owned data.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeEvent {
    RemotePtt(bool),
    Text(String),
    Dtmf(char),
    ConnectionLost,
    ConnectionRestored,
    /// The call ended; the engine's reason, as text.
    Hangup(String),
    Other(String),
}

#[derive(Default)]
struct Log {
    frames: Vec<RxFrame>,
    events: Vec<EventRecord>,
}

/// A linked IAX2 probe.
pub struct IaxProbe {
    call: Option<astar_iax::Call>,
    tx: TxFrames,
    format: VoiceFormat,
    rate: u32,
    answered_at: Instant,
    log: Arc<Mutex<Log>>,
    remote_keyed: Arc<AtomicBool>,
    up: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    drain: Option<JoinHandle<()>>,
}

impl IaxProbe {
    /// Dial and wait for the answer.
    ///
    /// # Errors
    /// [`ProbeError`] if the runtime cannot start, or the call is refused or
    /// not answered in time.
    pub fn dial(cfg: &IaxProbeConfig) -> Result<Self, ProbeError> {
        let profile = CallProfile {
            calling_number: Some(cfg.node.clone()),
            calling_name: Some(cfg.node.clone()),
            send_capability: true,
            codec_policy: cfg.codec_policy,
        };
        let RawDial {
            call,
            events,
            tx_frames,
            rx_frames,
        } = astar_iax::dial_raw_with_profile(
            cfg.server,
            cfg.node.clone(),
            cfg.dest.clone(),
            cfg.secret.clone(),
            profile,
        )?;

        let deadline = Instant::now() + cfg.answer_timeout;
        let format = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let _ = call.hangup(None);
                return Err(ProbeError::Timeout);
            }
            match events.recv_timeout(left) {
                Ok(CallEvent::Answered { format }) => break format,
                Ok(CallEvent::Hangup { reason }) => {
                    let _ = call.hangup(None);
                    return Err(ProbeError::Hungup(format!("{reason:?}")));
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = call.hangup(None);
                    return Err(ProbeError::Hungup("runtime gone".into()));
                }
            }
        };
        let answered_at = Instant::now();

        let log = Arc::new(Mutex::new(Log::default()));
        let remote_keyed = Arc::new(AtomicBool::new(false));
        let up = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let drain = {
            let shared = DrainShared {
                log: Arc::clone(&log),
                remote_keyed: Arc::clone(&remote_keyed),
                up: Arc::clone(&up),
                stop: Arc::clone(&stop),
            };
            std::thread::Builder::new()
                .name("iax-probe-rx".into())
                .spawn(move || drain_loop(&shared, &rx_frames, &events))
                .map_err(|e| ProbeError::Iax(IaxError::from(e)))?
        };

        Ok(Self {
            call: Some(call),
            tx: tx_frames,
            format,
            rate: cfg.codec_policy.max_sample_rate(),
            answered_at,
            log,
            remote_keyed,
            up,
            stop,
            drain: Some(drain),
        })
    }

    /// The negotiated voice codec.
    #[must_use]
    pub fn format(&self) -> VoiceFormat {
        self.format
    }

    /// Our PCM rate: what [`Self::transmit`] expects and recordings carry.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// When the call was answered.
    #[must_use]
    pub fn answered_at(&self) -> Instant {
        self.answered_at
    }

    /// `false` once the call has hung up (either side).
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.up.load(Ordering::Relaxed)
    }

    /// Whether the far end currently has PTT keyed toward us.
    #[must_use]
    pub fn remote_keyed(&self) -> bool {
        self.remote_keyed.load(Ordering::Relaxed)
    }

    /// Smoothed round-trip time, once measured.
    #[must_use]
    pub fn rtt(&self) -> Option<Duration> {
        self.call.as_ref().and_then(astar_iax::Call::rtt)
    }

    /// Key up, send `audio` (at [`Self::sample_rate`]) as 20 ms frames on an
    /// absolute schedule, then unkey. Blocks for the duration.
    ///
    /// # Errors
    /// [`IaxError`] if the call is gone.
    pub fn transmit(&self, audio_pcm: &[i16]) -> Result<TxReport, IaxError> {
        let call = self.call.as_ref().ok_or(IaxError::NoActiveCall)?;
        let frames = frames_of(audio_pcm, audio::frame_len(self.rate));
        call.set_ptt(true)?;
        let started = Instant::now();
        let frame = Duration::from_millis(u64::from(audio::FRAME_MS));
        for (i, f) in frames.iter().enumerate() {
            sleep_until(started + frame * u32::try_from(i).unwrap_or(u32::MAX));
            if self.tx.send(f.clone()).is_err() {
                return Err(IaxError::Io(std::io::Error::other("call runtime gone")));
            }
        }
        sleep_until(started + frame * u32::try_from(frames.len()).unwrap_or(u32::MAX));
        call.set_ptt(false)?;
        Ok(TxReport {
            started,
            finished: Instant::now(),
            frames: frames.len(),
        })
    }

    /// Send a DTMF digit.
    ///
    /// # Errors
    /// [`IaxError`] if the call is gone.
    pub fn send_dtmf(&self, digit: char) -> Result<(), IaxError> {
        self.call
            .as_ref()
            .ok_or(IaxError::NoActiveCall)?
            .send_dtmf(digit)
    }

    /// A point in the log to slice from later.
    #[must_use]
    pub fn mark(&self) -> Instant {
        Instant::now()
    }

    /// Frames received at or after `since`.
    #[must_use]
    pub fn frames_since(&self, since: Instant) -> Vec<RxFrame> {
        let l = self.log.lock().expect("probe log");
        l.frames.iter().filter(|f| f.at >= since).cloned().collect()
    }

    /// Events seen at or after `since`.
    #[must_use]
    pub fn events_since(&self, since: Instant) -> Vec<EventRecord> {
        let l = self.log.lock().expect("probe log");
        l.events.iter().filter(|e| e.at >= since).cloned().collect()
    }

    /// Wait `duration` and return what arrived during it.
    #[must_use]
    pub fn record(&self, duration: Duration) -> Vec<RxFrame> {
        let since = self.mark();
        std::thread::sleep(duration);
        self.frames_since(since)
    }

    /// Hang up (if still up) and stop the drain thread.
    ///
    /// # Errors
    /// [`IaxError`] from the hangup.
    pub fn hangup(mut self) -> Result<(), IaxError> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<(), IaxError> {
        let r = match self.call.take() {
            Some(c) => c.hangup(None),
            None => Ok(()),
        };
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.drain.take() {
            let _ = j.join();
        }
        r
    }
}

impl Drop for IaxProbe {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// What the drain thread shares with the probe.
struct DrainShared {
    log: Arc<Mutex<Log>>,
    remote_keyed: Arc<AtomicBool>,
    up: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

/// Take every received frame and call event off the call as it arrives,
/// stamp it, and append it to the log, until stopped.
fn drain_loop(
    sh: &DrainShared,
    rx_frames: &Receiver<astar_audio::RxFrame>,
    events: &Receiver<CallEvent>,
) {
    while !sh.stop.load(Ordering::Relaxed) {
        let got = rx_frames.recv_timeout(Duration::from_millis(10));
        let at = Instant::now();
        let mut log = sh.log.lock().expect("probe log");
        match got {
            Ok(frame) => log.frames.push(RxFrame {
                at,
                keyed: sh.remote_keyed.load(Ordering::Relaxed),
                seq: None,
                ts_ms: frame.clock.map(|c| c.ts_ms),
                samples: frame.pcm,
            }),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => sh.up.store(false, Ordering::Relaxed),
        }
        while let Ok(ev) = events.try_recv() {
            let event = match ev {
                CallEvent::RemotePtt(k) => {
                    sh.remote_keyed.store(k, Ordering::Relaxed);
                    ProbeEvent::RemotePtt(k)
                }
                CallEvent::Text(t) => ProbeEvent::Text(t),
                CallEvent::Dtmf(d) => ProbeEvent::Dtmf(d),
                CallEvent::ConnectionLost => ProbeEvent::ConnectionLost,
                CallEvent::ConnectionRestored => ProbeEvent::ConnectionRestored,
                CallEvent::Hangup { reason } => {
                    sh.up.store(false, Ordering::Relaxed);
                    ProbeEvent::Hangup(format!("{reason:?}"))
                }
                other => ProbeEvent::Other(format!("{other:?}")),
            };
            log.events.push(EventRecord { at, event });
        }
        drop(log);
        if !sh.up.load(Ordering::Relaxed) {
            // The call is gone: nothing more arrives, so idle cheaply (the
            // closed channel would otherwise return at once) until stopped.
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod tests {
    //! Loopback: the probe links into an in-process astar-iax listener whose
    //! answered call is adopted by a `Manager` over a hardware-free backend —
    //! the same path astar-server's inbound bridge takes. The backend's mic
    //! plays 700 Hz (heard by the probe when the node keys) and its speaker
    //! captures what the probe sends (1 kHz).

    use super::*;
    use astar_audio::{
        AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, MicId, OutputId,
        OutputSource, StreamConfig, StreamHandle,
    };
    use astar_iax::manager::Manager;
    use astar_iax::{
        IncomingAuthPolicy, IncomingCallEvent, IncomingCallListener, IncomingCallPolicy,
        IncomingDecisionPolicy,
    };
    use std::sync::atomic::AtomicBool;

    struct Worker {
        stop: Arc<AtomicBool>,
        join: Option<JoinHandle<()>>,
    }
    impl StreamHandle for Worker {
        fn stop(mut self: Box<Self>) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
        fn pause(&self) -> Result<(), AudioError> {
            Ok(())
        }
        fn resume(&self) -> Result<(), AudioError> {
            Ok(())
        }
    }

    /// Mic: a continuous -10 dBFS 700 Hz tone. Speaker: paced 20 ms reads,
    /// appended to `heard` as i16.
    struct BenchBackend {
        heard: Arc<Mutex<Vec<i16>>>,
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

    impl AudioBackend for BenchBackend {
        fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            Ok(vec![
                dev(Direction::Input, "in:bench"),
                dev(Direction::Output, "out:bench"),
            ])
        }
        fn default_input(&self) -> Option<DeviceInfo> {
            Some(dev(Direction::Input, "in:bench"))
        }
        fn default_output(&self) -> Option<DeviceInfo> {
            Some(dev(Direction::Output, "out:bench"))
        }
        fn open_input(
            &self,
            _d: &DeviceInfo,
            cfg: StreamConfig,
            mut sink: Box<dyn InputSink>,
            _overruns: Arc<std::sync::atomic::AtomicU64>,
        ) -> Result<Box<dyn StreamHandle>, AudioError> {
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stop);
            let rate = cfg.sample_rate;
            let join = std::thread::spawn(move || {
                let wave = audio::tone(700.0, -10.0, Duration::from_secs(1), rate);
                let n = audio::frame_len(rate);
                let start = Instant::now();
                let mut i = 0usize;
                while !stopping.load(Ordering::Relaxed) {
                    let off = (i * n) % wave.len();
                    let buf: Vec<f32> = wave[off..off + n]
                        .iter()
                        .map(|&v| f32::from(v) / 32768.0)
                        .collect();
                    sink.write(&buf, 0.3);
                    i += 1;
                    sleep_until(start + Duration::from_millis(20) * u32::try_from(i).unwrap());
                }
            });
            Ok(Box::new(Worker {
                stop,
                join: Some(join),
            }))
        }
        fn open_output(
            &self,
            _d: &DeviceInfo,
            cfg: StreamConfig,
            mut source: Box<dyn OutputSource>,
        ) -> Result<Box<dyn StreamHandle>, AudioError> {
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stop);
            let heard = Arc::clone(&self.heard);
            let n = audio::frame_len(cfg.sample_rate);
            let join = std::thread::spawn(move || {
                let start = Instant::now();
                let mut i = 0u32;
                let mut buf = vec![0f32; n];
                while !stopping.load(Ordering::Relaxed) {
                    let got = source.read(&mut buf);
                    buf[got..].fill(0.0);
                    #[allow(clippy::cast_possible_truncation)]
                    heard.lock().unwrap().extend(
                        buf.iter()
                            .map(|v| (v * 32767.0).clamp(-32768.0, 32767.0) as i16),
                    );
                    i += 1;
                    sleep_until(start + Duration::from_millis(20) * i);
                }
            });
            Ok(Box::new(Worker {
                stop,
                join: Some(join),
            }))
        }
    }

    #[test]
    fn probe_links_in_and_audio_flows_both_ways() {
        let policy = IncomingCallPolicy {
            decision: IncomingDecisionPolicy::AppDecide,
            auth: IncomingAuthPolicy::Off,
            ..IncomingCallPolicy::default()
        };
        let (listener, levents) = IncomingCallListener::builder()
            .bind("127.0.0.1:0".parse().unwrap())
            .policy(policy)
            .start()
            .expect("listener starts");
        let server = listener.local_addr();

        // The node side answers on its own thread while the probe's dial
        // blocks waiting for that answer.
        let heard = Arc::new(Mutex::new(Vec::<i16>::new()));
        let backend = BenchBackend {
            heard: Arc::clone(&heard),
        };
        let node = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(8);
            while Instant::now() < deadline {
                if let Ok(IncomingCallEvent::Incoming(c)) =
                    levents.recv_timeout(Duration::from_millis(50))
                {
                    let who = (c.calling_number.clone(), c.called_number.clone());
                    let (call, _ev) = c.answer().expect("answer");
                    let mut mgr = Manager::new(Box::new(backend));
                    let id = mgr.adopt(call, &OutputId::new("out:bench")).expect("adopt");
                    mgr.route(id, &MicId::new("in:bench")).expect("route");
                    return (who, mgr, id);
                }
            }
            panic!("no incoming call");
        });

        let probe = IaxProbe::dial(&IaxProbeConfig::new(server, "1998")).expect("probe links");
        let ((calling, called), mut mgr, id) = node.join().unwrap();
        assert_eq!(calling.as_deref(), Some("1997"), "presents its node number");
        assert_eq!(called.as_deref(), Some("1998"));
        assert_eq!(probe.format(), VoiceFormat::G711U);
        assert_eq!(probe.sample_rate(), 8000);
        assert!(probe.is_up());

        // Probe → node: 1 kHz for 1.5 s, heard on the node's speaker.
        std::thread::sleep(Duration::from_millis(300));
        heard.lock().unwrap().clear();
        let tx = probe
            .transmit(&audio::tone(
                1000.0,
                -10.0,
                Duration::from_millis(1500),
                8000,
            ))
            .unwrap();
        assert_eq!(tx.frames, 75);
        std::thread::sleep(Duration::from_millis(400));
        let at_node = std::mem::take(&mut *heard.lock().unwrap());
        let r = audio::detect_tone(&at_node, 8000, None).expect("node heard a tone");
        assert!((r.freq_hz - 1000.0).abs() < 10.0, "node heard {r:?}");
        assert!(r.level_dbfs > -30.0, "node heard {r:?}");

        // Node → probe: key the node's mic (700 Hz) for 1.5 s.
        let since = probe.mark();
        mgr.key(id).expect("key");
        std::thread::sleep(Duration::from_millis(1500));
        mgr.unkey(id).expect("unkey");
        std::thread::sleep(Duration::from_millis(400));
        let got = probe.frames_since(since);
        let pcm = crate::probe::concat(&got);
        let r = audio::detect_tone(&pcm, 8000, None).expect("probe heard a tone");
        assert!((r.freq_hz - 700.0).abs() < 10.0, "probe heard {r:?}");
        let onset = audio::onset_of_tone(&pcm, 8000, 700.0, -40.0).expect("onset");
        assert!(onset.offset < Duration::from_millis(500), "{onset:?}");
        let ts: Vec<i64> = got.iter().filter_map(|f| f.ts_ms).collect();
        if !ts.is_empty() {
            let loss = audio::loss_from_timestamps(&ts);
            assert!(loss.lost <= 2, "loopback loss {loss:?}");
        }
        assert!(
            probe
                .events_since(since)
                .iter()
                .any(|e| e.event == ProbeEvent::RemotePtt(true)),
            "probe saw the node key up"
        );

        probe.hangup().unwrap();
        let _ = mgr.hangup(id, None);
        drop(listener);
    }

    #[test]
    fn unanswered_dial_times_out() {
        // A bound socket that never answers.
        let mute = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut cfg = IaxProbeConfig::new(mute.local_addr().unwrap(), "1998");
        cfg.answer_timeout = Duration::from_millis(500);
        assert!(matches!(IaxProbe::dial(&cfg), Err(ProbeError::Timeout)));
    }
}
