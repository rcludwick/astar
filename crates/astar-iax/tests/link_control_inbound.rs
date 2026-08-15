// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Inbound-link validation composes with iax-8baf: the allowlist decides which
//! authenticated nodes may link. This test exercises the validator → reject/adopt
//! decision the app makes between `IncomingCallEvent::Incoming` and `adopt`,
//! using the validator directly (the Listener wiring is exercised in iax-8baf's
//! own tests; here we assert the link-layer decision surface).
use astar_iax::{KnownNodes, LinkAdmission, LinkValidator};

#[test]
fn an_authenticated_but_unlisted_node_is_rejected_for_linking() {
    // iax-8baf already proved identity; the allowlist is a SEPARATE gate.
    let v = LinkValidator::new(KnownNodes::from_iter_labels(["55553"]));
    // The caller's node label (from IncomingCall::calling_number / username).
    assert_eq!(v.validate("55553"), LinkAdmission::Accepted);
    match v.validate("40000") {
        LinkAdmission::Rejected { cause } => {
            // The app maps this to IncomingCall::reject(Some(cause)).
            assert!(cause.contains("allowlist"));
        }
        LinkAdmission::Accepted => panic!("unlisted node must not be admitted"),
    }
}
