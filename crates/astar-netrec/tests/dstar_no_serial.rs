// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The software vocoder never goes looking for a dongle. This is its own test
//! binary, holding one test, because it sets a process-wide variable that no
//! other test may race.
#![cfg(feature = "dstar-soft-ambe")]

use astar_dstar::Reflector;
use astar_netarchive::Ended;

mod common;
mod dstar_common;
use common::{SILENCE_RMS, Scratch, record, rms};
use dstar_common::{Talker, dstar_net, loopback, recorder_linked};

/// A path no USB scan can return, so nothing could be opened even if the
/// software path did go looking.
const BOGUS_PORT: &str = "/dev/cu.usbserial-NOSUCHDEVICE";

#[test]
fn dstar_never_opens_a_serial_port() {
    // SAFETY: this binary's only test, and this is its first line, so nothing
    // else is reading the environment yet.
    unsafe {
        std::env::set_var("IAX_THUMBDV_PORT", BOGUS_PORT);
    }
    let reflector = Reflector::bind(loopback()).expect("bind");
    let addr = reflector.local_addr();
    let handle = reflector.run();
    let scratch = Scratch::new("dstar-serial");

    let run = record(scratch.path(), &dstar_net(addr), 6, |session| {
        recorder_linked(&handle);
        let talker = Talker::link(addr, "NC0NTL");
        session.at(1.0);
        talker.talk(0x0001, "NC0NTL", 2);
    });
    handle.shutdown();

    assert!(
        matches!(run.meta.ended, Some(Ended::Window)),
        "ended {:?}",
        run.meta.ended
    );
    assert_eq!(run.talkers(), ["NC0NTL"], "rows: {:?}", run.rows);
    let over = &run.overs()[0];
    let level = rms(&run.over_samples(over));
    assert!(level > SILENCE_RMS, "{over:?} is silent: rms {level}");
    for error in run.errors() {
        assert!(
            !error.contains("ThumbDV"),
            "the software path reached for a dongle: {error}"
        );
    }
    assert_eq!(run.errors(), Vec::<String>::new(), "no error rows");
}
