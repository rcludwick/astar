// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Headless reproduction of the node-as-handset RX path (iax-64b6): a parrot
//! (`dial_raw`) calls a real `IncomingCallListener`, the call is adopted+routed
//! into a Manager, the parrot sends loud PCM voice, and we assert that audio
//! actually reaches the node's OUTPUT (speaker) via a capturing backend. This is
//! the "I'm not hearing anything" direction (parrot -> node -> speaker).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, MicId, OutputId,
    OutputSource, StreamConfig, StreamHandle,
};
use astar_iax::manager::Manager;
use astar_iax::{
    CallEvent, CallMode, IncomingAuthPolicy, IncomingCallEvent, IncomingCallListener,
    IncomingCallPolicy, IncomingDecisionPolicy, ParrotConfig, dial_raw, run_parrot,
};

/// A backend that drives the output (like a real device callback) and records
/// the peak amplitude it pulls from the mixer — so a test can prove audio
/// reaches the speaker. Input is silent.
struct CapturingBackend {
    peak: Arc<Mutex<f32>>,
    /// Count of output reads that carried audible audio (peak > 0.1). One frame
    /// barely registers; a real stream registers many.
    loud_reads: Arc<AtomicUsize>,
}

struct ThreadHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}
impl StreamHandle for ThreadHandle {
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

/// A backend whose INPUT emits a loud 440 Hz tone (so the node "talks" to the
/// parrot) and whose OUTPUT records the peak it plays. Used to exercise the full
/// record-then-playback flow through `run_parrot`.
struct ToneCaptureBackend {
    out_peak: Arc<Mutex<f32>>,
    out_loud_reads: Arc<AtomicUsize>,
}

impl AudioBackend for ToneCaptureBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "in:cap"),
            dev(Direction::Output, "out:cap"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "in:cap"))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "out:cap"))
    }
    fn open_input(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        mut sink: Box<dyn InputSink>,
        _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let join = std::thread::spawn(move || {
            // Phase in seconds, advanced 1/8000 per sample; wraps at 1.0.
            let mut t: f32 = 0.0;
            while !stop_t.load(Ordering::Relaxed) {
                // 20 ms = 160 samples of a 440 Hz sine at 0.5 amplitude.
                let mut buf = [0f32; 160];
                for s in &mut buf {
                    *s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
                    t += 1.0 / 8000.0;
                    if t >= 1.0 {
                        t -= 1.0;
                    }
                }
                sink.write(&buf, 0.5);
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Ok(Box::new(ThreadHandle {
            stop,
            join: Some(join),
        }))
    }
    fn open_output(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        mut source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        let peak = Arc::clone(&self.out_peak);
        let loud = Arc::clone(&self.out_loud_reads);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let join = std::thread::spawn(move || {
            let mut buf = vec![0f32; 160];
            while !stop_t.load(Ordering::Relaxed) {
                let n = source.read(&mut buf);
                let p = buf[..n].iter().fold(0f32, |m, &s| m.max(s.abs()));
                if p > *peak.lock().unwrap() {
                    *peak.lock().unwrap() = p;
                }
                if p > 0.1 {
                    loud.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Ok(Box::new(ThreadHandle {
            stop,
            join: Some(join),
        }))
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

impl AudioBackend for CapturingBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "in:cap"),
            dev(Direction::Output, "out:cap"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "in:cap"))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "out:cap"))
    }
    fn open_input(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        _s: Box<dyn InputSink>,
        _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Ok(Box::new(NullHandle))
    }
    fn open_output(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        mut source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        let peak = Arc::clone(&self.peak);
        let loud = Arc::clone(&self.loud_reads);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let join = std::thread::spawn(move || {
            let mut buf = vec![0f32; 160]; // 20 ms @ 8 kHz mono
            while !stop_thread.load(Ordering::Relaxed) {
                let n = source.read(&mut buf);
                let p = buf[..n].iter().fold(0f32, |m, &s| m.max(s.abs()));
                if p > *peak.lock().unwrap() {
                    *peak.lock().unwrap() = p;
                }
                if p > 0.1 {
                    loud.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Ok(Box::new(ThreadHandle {
            stop,
            join: Some(join),
        }))
    }
}

#[test]
fn parrot_audio_reaches_the_node_output() {
    // Node side: a listener that surfaces the offer (we answer it ourselves).
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
    let addr = listener.local_addr();

    // Parrot side: a device-free raw dial into the node.
    let raw = dial_raw(addr, "parrot", "s", "", CallMode::Standard).expect("dial");

    // Node Manager over the capturing backend.
    let peak = Arc::new(Mutex::new(0.0f32));
    let loud_reads = Arc::new(AtomicUsize::new(0));
    let backend = CapturingBackend {
        peak: Arc::clone(&peak),
        loud_reads: Arc::clone(&loud_reads),
    };
    let mut mgr = Manager::new(Box::new(backend));

    // Answer the offer and bridge it: adopt -> output bus (capturing) + route a mic.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut answered = false;
    let mut bridged = false;
    while Instant::now() < deadline && !answered {
        if !bridged && let Ok(IncomingCallEvent::Incoming(c)) = levents.try_recv() {
            let (call, _e) = c.answer().expect("answer");
            let id = mgr.adopt(call, &OutputId::new("out:cap")).expect("adopt");
            mgr.route(id, &MicId::new("in:cap")).expect("route");
            bridged = true;
        }
        while let Ok(ev) = raw.events.try_recv() {
            if let CallEvent::Answered { .. } = ev {
                answered = true;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(answered, "parrot must connect");
    assert!(bridged, "node must adopt+route the call");

    // Mimic the live parrot: idle for several seconds (record + wait), THEN
    // burst the playback. This is what differs from the continuous case.
    std::thread::sleep(Duration::from_secs(4));

    // Parrot sends ~1.5 s of loud PCM voice (a large constant sample level).
    let loud = vec![16_000_i16; 160];
    let send_until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < send_until {
        let _ = raw.tx_frames.send(loud.clone());
        std::thread::sleep(Duration::from_millis(20));
    }
    // Let the tail drain through the jitterbuffer + capturing thread.
    std::thread::sleep(Duration::from_millis(500));

    let measured = *peak.lock().unwrap();
    let reads = loud_reads.load(Ordering::Relaxed);
    let _ = raw.call.hangup(None);
    assert!(
        measured > 0.1,
        "parrot voice must reach the node output (peak={measured:.3}, floor≈0)"
    );
    // The CONTINUITY check: ~1.5 s of voice is ~75 frames. The listener used to
    // drop every mini frame (all voice after the first FULL frame), so only ~1
    // read was audible — silent in practice. Require a real stream.
    //
    // Threshold is intentionally loose (> 5, not the per-frame count). The
    // capturing thread sleeps 5 ms/read and competes with the parallel test for
    // scheduling; on a loaded headless CI runner it gets starved and accumulates
    // far fewer audible reads (observed 13-15) than on an idle dev Mac, even
    // though the full stream is present (the peak assertion above proves audio
    // arrives). Any value well above 1 disproves the mini-frame-drop regression.
    assert!(
        reads > 5,
        "the whole voice stream must reach the output, not just the first frame (loud_reads={reads})"
    );
}

#[test]
fn full_record_playback_through_run_parrot_reaches_node_output() {
    // The exact harness scenario: a node "talks" (tone mic) to a parrot driven
    // by run_parrot, which records then replays. Assert the replay reaches the
    // node OUTPUT (what the operator would hear).
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
    let addr = listener.local_addr();

    let raw = dial_raw(addr, "parrot", "s", "", CallMode::Standard).expect("dial");

    let out_peak = Arc::new(Mutex::new(0.0f32));
    let out_loud_reads = Arc::new(AtomicUsize::new(0));
    let backend = ToneCaptureBackend {
        out_peak: Arc::clone(&out_peak),
        out_loud_reads: Arc::clone(&out_loud_reads),
    };
    let mut mgr = Manager::new(Box::new(backend));

    // Answer + bridge.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut bridged = None;
    while Instant::now() < deadline && bridged.is_none() {
        if let Ok(IncomingCallEvent::Incoming(c)) = levents.try_recv() {
            let (call, _e) = c.answer().expect("answer");
            let id = mgr.adopt(call, &OutputId::new("out:cap")).expect("adopt");
            mgr.route(id, &MicId::new("in:cap")).expect("route");
            bridged = Some(id);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let id = bridged.expect("call bridged");

    // Drive the parrot (record → wait → playback) on its own thread.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = Arc::clone(&stop);
    let parrot = std::thread::spawn(move || {
        let cfg = ParrotConfig {
            playback_delay: Duration::from_secs(1),
            silence_gap: Duration::from_millis(500),
        };
        run_parrot(raw, &cfg, &stop_t, |_l| {});
    });

    // Node keys for ~1.5s (tone -> parrot records), then unkeys.
    mgr.key(id).expect("key");
    std::thread::sleep(Duration::from_millis(1500));
    mgr.unkey(id).expect("unkey");

    // Wait out the playback_delay (1s) + replay (~1.5s) + margin. The node
    // output only sees audio during the replay (it's silent while the node is
    // the one talking), so any peak rise proves the replay was heard.
    std::thread::sleep(Duration::from_secs(4));
    let measured = *out_peak.lock().unwrap();
    let reads = out_loud_reads.load(Ordering::Relaxed);

    stop.store(true, Ordering::Relaxed);
    let _ = parrot.join();
    let _ = mgr.hangup(id, None);

    assert!(
        measured > 0.1,
        "run_parrot's replay must reach the node output (peak={measured:.3})"
    );
    // The replay is ~1.5 s (~75 frames). Before the mini-frame demux fix the
    // listener dropped all but the first, so only ~1 read was audible.
    //
    // Threshold is intentionally loose (> 5, not the per-frame count). The
    // capturing thread sleeps 5 ms/read and competes with the parallel test for
    // scheduling; on a loaded headless CI runner it gets starved and accumulates
    // far fewer audible reads (observed 13-15) than on an idle dev Mac, even
    // though the full stream is present (the peak assertion above proves audio
    // arrives). Any value well above 1 disproves the mini-frame-drop regression.
    assert!(
        reads > 5,
        "the whole replay must reach the output, not just the first frame (loud_reads={reads})"
    );
}
