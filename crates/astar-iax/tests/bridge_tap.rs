// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The public stream's tap is an ordinary non-contributing bridge member.
//! These pin what the stream relies on: it hears the members' audio, it
//! does not hear an announcement played to someone else, parrot mode
//! refuses it, and a Handset switch drops it in a way the holder can see.
//! No network, no audio hardware.

use std::time::{Duration, Instant};

use astar_iax::{
    AnnouncePolicyReq, AnnounceRequest, BridgeConfig, BridgeMode, Destination, Manager, Phrase,
};

fn manager(mode: BridgeMode) -> Manager {
    let mut mgr = Manager::new(Box::new(astar_audio::NullBackend::new()));
    mgr.set_bridge_config(BridgeConfig {
        mode,
        mix_minus: true,
        include_local_radio: false,
        ..Default::default()
    })
    .unwrap();
    mgr
}

fn tone(rate: u32, hz: f32, n: usize, offset: usize) -> Vec<i16> {
    (0..n)
        .map(|i| {
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let v = (((offset + i) as f32 * hz * std::f32::consts::TAU / rate as f32).sin()
                * 8000.0) as i16;
            v
        })
        .collect()
}

fn goertzel_dbfs(pcm: &[i16], rate: u32, hz: f32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let w = 2.0 * (std::f32::consts::TAU * hz / rate as f32).cos();
    let (mut a, mut b) = (0.0f64, 0.0f64);
    for &x in pcm {
        let c = f64::from(x) / 32768.0 + f64::from(w) * a - b;
        b = a;
        a = c;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = pcm.len().max(1) as f64;
    let power = (a * a + b * b - f64::from(w) * a * b) / (n * n / 4.0);
    #[allow(clippy::cast_possible_truncation)]
    let db = (10.0 * power.max(1e-12).log10()) as f32;
    db
}

#[test]
fn tap_hears_member_audio_but_not_announcements() {
    let mut mgr = manager(BridgeMode::Bridge);
    // A member feeding a 1 kHz tone into the relay (an M17 leg has this shape).
    let caller = mgr
        .add_source_member()
        .expect("bridge mode enrolls a source");
    // Another receiving member, whom the node greets with a 2 kHz announcement.
    let greeted = mgr.add_bridge_member().expect("a listener enrolls");
    let tap = mgr.add_bridge_member().expect("the tap enrolls");
    let rate = tap.sample_rate;
    let frame = usize::try_from(rate / 50).unwrap();

    let pcm: std::sync::Arc<[i16]> = tone(rate, 2000.0, usize::try_from(rate).unwrap(), 0).into();
    mgr.announce_to_bridge_member(
        greeted.id,
        AnnounceRequest {
            phrase: Phrase::Pcm(pcm),
            destination: Destination::ToAir,
            policy: AnnouncePolicyReq::Seize,
            priority: 4,
        },
        Duration::ZERO,
    )
    .expect("the announcement is queued for the greeted member");

    let mut heard: Vec<i16> = Vec::new();
    let mut greeted_heard: Vec<i16> = Vec::new();
    let mut sent = 0usize;
    let deadline = Instant::now() + Duration::from_secs(5);
    while heard.len() < usize::try_from(rate).unwrap() && Instant::now() < deadline {
        caller
            .to_conference
            .send(astar_audio::RxFrame::from(tone(
                rate,
                1000.0,
                frame,
                sent * frame,
            )))
            .unwrap();
        sent += 1;
        if let Ok(f) = tap.from_conference.recv_timeout(Duration::from_millis(30)) {
            heard.extend(f);
        }
        while let Ok(f) = greeted.from_conference.try_recv() {
            greeted_heard.extend(f);
        }
    }
    assert!(
        heard.len() >= usize::try_from(rate).unwrap() / 2,
        "the mixer ticked {} samples",
        heard.len()
    );
    let tail = &heard[heard.len() / 4..];
    let one_k = goertzel_dbfs(tail, rate, 1000.0);
    let two_k = goertzel_dbfs(tail, rate, 2000.0);
    assert!(
        one_k > -40.0,
        "the tap hears the member: 1 kHz at {one_k:.1} dBFS"
    );
    assert!(
        two_k < -60.0,
        "no announcement in the tap: 2 kHz at {two_k:.1} dBFS"
    );
    // Not vacuous: the greeted member really was played the announcement.
    assert!(
        goertzel_dbfs(&greeted_heard, rate, 2000.0) > -40.0,
        "the announcement was audible to its target"
    );
}

#[test]
fn parrot_mode_refuses_the_tap() {
    let mut mgr = manager(BridgeMode::Parrot);
    assert!(mgr.add_bridge_member().is_none());
}

#[test]
fn handset_switch_unenrols_the_tap() {
    let mut mgr = manager(BridgeMode::Bridge);
    let tap = mgr.add_bridge_member().expect("bridge mode enrolls");
    assert!(mgr.has_bridge_member(tap.id));
    mgr.set_bridge_config(BridgeConfig {
        mode: BridgeMode::Handset,
        mix_minus: true,
        include_local_radio: false,
        ..Default::default()
    })
    .unwrap();
    assert!(
        !mgr.has_bridge_member(tap.id),
        "Handset dropped the conference"
    );
    // Back to Bridge: the new conference reuses the id for someone else; a
    // stale entry would let the old holder think it is still enrolled.
    mgr.set_bridge_config(BridgeConfig {
        mode: BridgeMode::Bridge,
        mix_minus: true,
        include_local_radio: false,
        ..Default::default()
    })
    .unwrap();
    let _src = mgr.add_source_member().expect("a source takes id 0");
    assert!(!mgr.has_bridge_member(tap.id));
    let again = mgr.add_bridge_member().expect("the tap re-enrols");
    assert!(mgr.has_bridge_member(again.id));
    mgr.remove_bridge_member(again.id);
    assert!(!mgr.has_bridge_member(again.id));
}
