// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! An SSTV picture heard during a session reaches the recordings page.
//!
//! The session is driven tick by tick with the test signal, as the runner
//! drives it with what the network hands over, and the page's JSON is read
//! the way the recorder's own web server reads it. Nothing leaves the
//! machine: no reflector is reached and nothing is sent.

mod common;
#[path = "common/sstv_signal.rs"]
mod sstv_signal;

use std::time::{Duration, Instant};

use astar_netarchive::{Access, Archive, Ended, NetsReply, NetsRequest, Pages, ViewOpts};
use astar_netrec::config::Target;
use astar_netrec::feed::Frame;
use astar_netrec::schedule::Window;
use astar_netrec::session::SessionRecorder;

const RATE: u32 = 8_000;
const FRAME: usize = 160;

#[test]
fn sstv_session_shows_an_image_item() {
    let scratch = common::Scratch::new("sstv");
    let net = common::net(Target::M17 {
        host: "127.0.0.1".into(),
        port: 17000,
        module: 'C',
    });
    let start: jiff::Timestamp = "2026-11-03T19:00:00Z".parse().unwrap();
    let window = Window {
        net: 0,
        start,
        end: start,
        join_at: start,
        leave_at: start,
    };
    let zoned = |n: u64| {
        start
            .checked_add(jiff::SignedDuration::from_millis(
                i64::try_from(n * 20).unwrap(),
            ))
            .unwrap()
            .to_zoned(net.tz.clone())
    };
    let mut rec =
        SessionRecorder::open(scratch.path(), &net, &window, RATE, zoned(0)).expect("open");
    rec.set_sstv_lossless(true);

    // A second of quiet, the picture, then three seconds of quiet.
    let mut audio = vec![0i16; RATE as usize];
    audio.extend(sstv_signal::robot36(RATE));
    audio.extend(std::iter::repeat_n(0, 3 * RATE as usize));
    let base = Instant::now();
    let mut n = 0u64;
    for chunk in audio.chunks(FRAME) {
        let mut samples = chunk.to_vec();
        samples.resize(FRAME, 0);
        let voiced = samples.iter().any(|s| *s != 0);
        let frame = voiced.then_some(Frame {
            rate: RATE,
            samples,
        });
        let at = base + Duration::from_millis(n * 20);
        rec.tick(frame.as_ref(), Some("N0CALL"), at, zoned(n))
            .expect("tick");
        n += 1;
    }
    rec.close(Ended::Window, 0, 0, zoned(n)).expect("close");

    let entry = Archive::new(scratch.path().to_path_buf())
        .scan()
        .into_iter()
        .next()
        .expect("the session");
    let pages = Pages::new(scratch.path().to_path_buf(), ViewOpts::default());
    let path = format!("/nets/{}/{}/data.json", entry.net_slug, entry.session_slug);
    let NetsReply::Response(r) = pages.handle(&NetsRequest {
        method: "GET",
        path: &path,
        access: Access::Admin,
        offer_sign_in: false,
    }) else {
        panic!("data.json is no file");
    };
    assert_eq!(r.status, 200);
    let text = String::from_utf8(r.body).unwrap();
    assert_eq!(text.matches(r#""type":"image""#).count(), 1, "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let image = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "image")
        .unwrap();
    assert_eq!(image["mode"], "Robot36");
    assert_eq!(image["call"], "N0CALL");
}
