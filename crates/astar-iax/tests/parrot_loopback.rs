// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! End-to-end slin16 parrot loopback (iax-feab Task 5): a headless dial-out
//! parrot leg (`dial_raw_with_policy`, `PreferSlin16`) calls a real
//! `IncomingCallListener` (`AutoAccept`, auth `Off`, `codec_policy =
//! PreferSlin16`); the accepted call is adopted into a `Manager` running
//! `BridgeMode::Parrot`. The parrot sends loud 16 kHz PCM; the Manager's
//! record/replay/report pump (driven by `poll_announcements`) plays it back on
//! the SAME leg, then hangs up once the (TTS-disabled-in-tests) spoken report
//! resolves to nothing.
//!
//! Mirrors `tests/node_audio_path.rs`'s Manager+listener fixture pattern, but
//! goes device-free on BOTH ends (`dial_raw`'s raw frame channels on the caller
//! side; a null `AudioBackend` on the node side) — `BridgeMode::Parrot` rides
//! the Conference engine, which never opens the adopted call's `OutputId` as a
//! real device (see `Manager::adopt`'s conference branch), so a backend that
//! never touches hardware is sufficient here.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputId, OutputSource,
    StreamConfig, StreamHandle,
};
use astar_iax::manager::Manager;
use astar_iax::{
    BridgeConfig, BridgeMode, CallEvent, CallMode, CodecPolicy, IncomingAuthPolicy,
    IncomingCallEvent, IncomingCallListener, IncomingCallPolicy, IncomingDecisionPolicy,
    dial_raw_with_policy,
};
use astar_iax_core::VoiceFormat;

// ---------------------------------------------------------------------------
// Null audio backend: `BridgeMode::Parrot` adopts via the Conference engine,
// which never calls `open_output`/`open_input` for the adopted leg (see
// `Manager::adopt`), so this never needs to do anything but exist.
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

struct NullBackend;

fn dev(direction: Direction, tag: &str) -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new(tag.to_string()),
        name: tag.to_string(),
        direction,
        channels: 1,
        native_sample_rates: vec![16_000],
    }
}

impl AudioBackend for NullBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "in:s"),
            dev(Direction::Output, "out:s"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "in:s"))
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
        _source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Ok(Box::new(NullHandle))
    }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Bring up a listener (`AutoAccept`, auth `Off`, `PreferSlin16`) and a
/// `Manager` in `BridgeMode::Parrot` with tight tuning, dial into it with
/// `dial_raw_with_policy(.., PreferSlin16)`, and drive both the raw dial's
/// event loop and `mgr.poll_announcements()` from one thread — the test owns
/// the pump cadence, as the brief specifies.
#[test]
fn slin16_parrot_echoes_then_hangs_up() {
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AutoAccept,
        auth: IncomingAuthPolicy::Off,
        codec_policy: CodecPolicy::PreferSlin16,
        ..IncomingCallPolicy::default()
    };
    let (listener, levents) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let addr: SocketAddr = listener.local_addr();

    let mut mgr = Manager::with_policy(Box::new(NullBackend), CodecPolicy::PreferSlin16);
    mgr.set_bridge_config(BridgeConfig {
        mode: BridgeMode::Parrot,
        mix_minus: true,
        include_local_radio: false,
        parrot: Some(astar_audio::ParrotTuning {
            playback_delay_ticks: 2,
            silence_gap_ticks: 2,
            vox_threshold_db: -40.0,
            max_record_ticks: 500,
        }),
    })
    .expect("parrot bridge config applies");
    assert!(mgr.conference_active(), "Parrot mode starts the engine");

    let raw = dial_raw_with_policy(
        addr,
        "parrot-e2e",
        "s",
        "",
        CallMode::Standard,
        CodecPolicy::PreferSlin16,
    )
    .expect("dial");

    // Adopt the accepted leg into the Manager as soon as the listener
    // surfaces it. `AutoAccept` delivers `IncomingCallEvent::Answered`
    // directly (no app decision).
    let out = OutputId::new("out:s");
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut adopted = false;
    while Instant::now() < deadline && !adopted {
        if let Ok(IncomingCallEvent::Answered { call, .. }) = levents.try_recv() {
            mgr.adopt(call, &out).expect("adopt into parrot bridge");
            adopted = true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(adopted, "the listener must surface the auto-answered leg");

    // The call must be up and negotiated slin16 while it's alive.
    let negotiated_deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_slin16 = false;
    while Instant::now() < negotiated_deadline && !saw_slin16 {
        if raw.call.snapshot().negotiated_format == Some(VoiceFormat::Slin16) {
            saw_slin16 = true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        saw_slin16,
        "the dialed leg must negotiate Slin16 under PreferSlin16 on both ends"
    );

    // Send 5 loud 20 ms/16 kHz frames (320 samples each) — well above the VOX
    // floor, so the conference member's take starts and (once the frames stop
    // arriving) ends on the silence-gap tick count.
    let loud: Vec<i16> = vec![9000_i16; 320];
    for _ in 0..5 {
        raw.tx_frames.send(loud.clone()).expect("tx frame sent");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Drive the pump loop: poll_announcements() advances the parrot's
    // record -> replay -> report -> hangup state machine. Each iteration
    // drains `rx_frames` (the echo) BEFORE `events` (the terminal Hangup), so
    // asserting `saw_echo` at the moment Hangup is observed proves the IN
    // ORDER requirement: the echo must already have arrived.
    let mut saw_echo = false;
    let mut saw_hangup = false;
    let pump_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < pump_deadline && !saw_hangup {
        mgr.poll_announcements();

        while let Ok(frame) = raw.rx_frames.try_recv() {
            if !saw_echo && frame.iter().any(|&s| s.abs() >= 8990) {
                saw_echo = true;
            }
        }

        while let Ok(ev) = raw.events.try_recv() {
            if let CallEvent::Hangup { .. } = ev {
                assert!(
                    saw_echo,
                    "Hangup must not arrive before the echo — the parrot replays \
                     the take, THEN speaks the (unresolvable, TTS-off) report and \
                     hangs up"
                );
                saw_hangup = true;
            }
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        saw_echo,
        "the parrot must replay the loud take back to the dialer (amplitude >= 8990)"
    );
    assert!(
        saw_hangup,
        "the parrot must hang up the leg once the report pump runs \
         (TTS is disabled in tests, so the report resolves to nothing and \
         hangup follows immediately)"
    );

    let _ = raw.call.hangup(None);
}
