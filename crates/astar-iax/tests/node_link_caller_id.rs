// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! iax-cnlink: a node-to-node link that astar-server dials must identify
//! itself the way an `app_rpt` node does, with its own node number as the
//! caller ID. `app_rpt` hangs up 0.4-0.6 ms after ACCEPT on a link whose caller
//! ID is not numeric ("Node  invalid for connection: Caller ID is not
//! numeric"), which nodebench saw on every link astar-server dialled.
//!
//! The listener here stands in for the far node: 127.0.0.1 only.

use std::time::Duration;

use astar_iax::{
    CallMode, IncomingAuthPolicy, IncomingCallEvent, IncomingCallListener, IncomingCallPolicy,
    IncomingDecisionPolicy, dial_raw,
};

#[test]
fn node_link_new_carries_our_node_number_as_calling_number() {
    let (listener, events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(IncomingCallPolicy {
            decision: IncomingDecisionPolicy::AppDecide,
            auth: IncomingAuthPolicy::Off,
            ..IncomingCallPolicy::default()
        })
        .start()
        .expect("listener starts");

    let _raw = dial_raw(
        listener.local_addr(),
        "1998",
        "1999",
        "",
        CallMode::NodeLink {
            node: "1998".into(),
        },
    )
    .expect("dial");

    let incoming = match events.recv_timeout(Duration::from_secs(3)) {
        Ok(IncomingCallEvent::Incoming(c)) => c,
        Ok(IncomingCallEvent::Answered { .. }) => panic!("AppDecide yields Incoming"),
        Err(e) => panic!("no NEW reached the listener: {e}"),
    };
    assert_eq!(
        incoming.calling_number.as_deref(),
        Some("1998"),
        "the NEW must carry CALLING_NUMBER = our node number"
    );
    assert_eq!(incoming.called_number.as_deref(), Some("1999"));
    assert_eq!(incoming.username.as_deref(), Some("1998"));
}
