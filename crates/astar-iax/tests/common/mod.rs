// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Shared test helpers for Manager-level integration tests (iax-6c5d).
//!
//! Provides ready-to-use `(Manager, CallId)` pairs backed by a null audio
//! backend so tests never open real audio devices.

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, OnceLock};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, MicId, OutputId,
    OutputSource, StreamConfig, StreamHandle,
};
use astar_iax::manager::{DialSpec, Manager};
use astar_iax::{
    AnnouncePolicyReq, AnnounceRequest, CallId, CallMode, CodecPolicy, Destination, Phrase,
};

// ---------------------------------------------------------------------------
// Null (no-op) backend: opens silently, never touches real hardware.
// ---------------------------------------------------------------------------

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

struct MultiNullBackend;

fn dev(dir: Direction, tag: &str) -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new(tag.to_string()),
        name: tag.to_string(),
        direction: dir,
        channels: 1,
        native_sample_rates: vec![8_000],
    }
}

impl AudioBackend for MultiNullBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "in:a"),
            dev(Direction::Output, "out:s"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "in:a"))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "out:s"))
    }
    fn open_input(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        _sink: Box<dyn InputSink>,
        _overruns: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Ok(Box::new(NullHandle))
    }
    fn open_output(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        _src: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Ok(Box::new(NullHandle))
    }
}

// ---------------------------------------------------------------------------
// ControlledBackend: retains the opened sink so tests can push mic frames.
// ---------------------------------------------------------------------------

struct ControlledShared {
    sink: Option<Box<dyn InputSink>>,
}

/// Cloneable control surface that survives the backend being moved into the
/// manager. Call [`NullControls::push_mic_frames`] to simulate capture
/// callbacks — this drives the announcement drain path.
#[derive(Clone)]
pub struct NullControls(Arc<Mutex<ControlledShared>>);

impl NullControls {
    /// Push `n` silent 160-sample frames through the retained mic sink,
    /// simulating `n` capture callbacks from the audio hardware.
    pub fn push_mic_frames(&self, n: usize) {
        let silence = vec![0.0_f32; 160];
        for _ in 0..n {
            let mut g = self.0.lock().unwrap();
            if let Some(mut sink) = g.sink.take() {
                drop(g);
                sink.write(&silence, 0.0);
                self.0.lock().unwrap().sink = Some(sink);
            }
        }
    }
}

struct ControlledBackend(Arc<Mutex<ControlledShared>>);

impl ControlledBackend {
    fn new() -> (Self, NullControls) {
        let shared = Arc::new(Mutex::new(ControlledShared { sink: None }));
        (Self(Arc::clone(&shared)), NullControls(shared))
    }
}

impl AudioBackend for ControlledBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "in:a"),
            dev(Direction::Output, "out:s"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "in:a"))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "out:s"))
    }
    fn open_input(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        sink: Box<dyn InputSink>,
        _overruns: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        // Retain the sink so tests can drive it.
        self.0.lock().unwrap().sink = Some(sink);
        Ok(Box::new(NullHandle))
    }
    fn open_output(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        _src: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Ok(Box::new(NullHandle))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A fake peer that won't answer — the call stays in `Connecting` state, but
/// the routing machinery (open mic lane, output bus) is fully functional for
/// testing `announce`.
///
/// iax-2894: this MUST be a real bound-but-silent socket, not a closed
/// `127.0.0.1:4569`. Sending UDP to a *closed* localhost port triggers an
/// ICMP "port unreachable" that the OS surfaces as `ECONNREFUSED` on the
/// dialing socket's next `recv_from`; the call run-loop treats that as a fatal
/// network error and exits within milliseconds. Then `announce`'s auto-key
/// sends `set_ptt` to the dead run-loop, `Call::send` maps the closed channel
/// to `NoActiveCall`, and the announce `unwrap()`s panic — flakily, depending
/// on whether the ICMP error landed before the test's `announce` call. A real
/// bound socket that simply never replies keeps the port open (no ICMP), so
/// the call stays in `NewSent`/Connecting for its full ~10 s retry budget —
/// far longer than any test — and the run-loop is reliably alive.
fn fake_peer() -> SocketAddr {
    static SINK: OnceLock<SocketAddr> = OnceLock::new();
    *SINK.get_or_init(|| {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind fake-peer sink");
        let addr = sock.local_addr().expect("sink addr");
        // Leak the socket so the port stays open (silent) for the whole test
        // process — dropping it would close the port and reintroduce the ICMP
        // refusal this fix exists to avoid.
        std::mem::forget(sock);
        addr
    })
}

fn dial_spec(id: u64, output: &str) -> DialSpec {
    DialSpec {
        id: CallId::from_raw(id),
        node: format!("test-node-{id}"),
        peer: fake_peer(),
        output: OutputId::new(output),
        caller_id: "test-harness".to_string(),
        secret: String::new(),
        mode: CallMode::Standard,
        dest: "1000".to_string(),
        frame_observer: None,
        codec_policy: CodecPolicy::default(),
    }
}

/// A `Manager` with one call that has a routed mic (so `ToAir` announcements
/// work). The call is in `Connecting` state (fake peer, no real socket
/// handshake), but the mic lane and output bus are fully open.
#[allow(dead_code)]
pub fn manager_with_routed_call() -> (Manager, CallId) {
    let mut mgr = Manager::new(Box::new(MultiNullBackend));
    let id = mgr.dial(dial_spec(1, "out:s")).expect("dial ok");
    mgr.route(id, &MicId::new("in:a")).expect("route ok");
    (mgr, id)
}

/// A `Manager` with one call that has NO mic routed (monitor-only), so
/// `ToAir` announcements must fail with `AnnounceUnavailable`.
#[allow(dead_code)]
pub fn manager_with_monitor_only_call() -> (Manager, CallId) {
    let mut mgr = Manager::new(Box::new(MultiNullBackend));
    let id = mgr.dial(dial_spec(2, "out:s")).expect("dial ok");
    // Deliberately NOT calling route() — stays monitor-only.
    (mgr, id)
}

/// A `Manager` with one call + a `NullControls` handle that can push mic
/// frames, enabling `drive_capture` / `poll_announcements` integration tests.
#[allow(dead_code)]
pub fn manager_with_controlled_call() -> (Manager, CallId, NullControls) {
    let (backend, controls) = ControlledBackend::new();
    let mut mgr = Manager::new(Box::new(backend));
    let id = mgr.dial(dial_spec(1, "out:s")).expect("dial ok");
    mgr.route(id, &MicId::new("in:a")).expect("route ok");
    (mgr, id, controls)
}

/// Build a `ToAir`/`Seize`/priority-5 request from raw PCM.
#[allow(dead_code)]
pub fn pcm_to_air(pcm: Arc<[i16]>) -> AnnounceRequest {
    AnnounceRequest {
        phrase: Phrase::Pcm(pcm),
        destination: Destination::ToAir,
        policy: AnnouncePolicyReq::Seize,
        priority: 5,
    }
}

/// Returns `true` if `call` is currently keyed in `mgr`'s snapshot.
#[allow(dead_code)]
pub fn is_keyed(mgr: &Manager, call: CallId) -> bool {
    mgr.snapshot()
        .calls
        .iter()
        .find(|c| c.id == call)
        .is_some_and(|c| c.keyed)
}

/// Push `n` silent 160-sample capture callbacks through `controls` to drain
/// in-flight announcements.
#[allow(dead_code)]
pub fn drive_capture(controls: &NullControls, n: usize) {
    controls.push_mic_frames(n);
}
