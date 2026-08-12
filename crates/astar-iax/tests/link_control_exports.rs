// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Public-surface guard: the link-control API is re-exported at the crate root.
use astar_iax::{KnownNodes, LinkAdmission, LinkEvent, LinkSpec, LinkValidator};

#[test]
fn link_control_types_are_public_at_crate_root() {
    let _ = LinkSpec {
        node: "55553".into(),
        mode: astar_iax::LinkMode::Monitor,
        output: astar_audio::OutputId::new("out:s"),
        caller_id: "1999".into(),
        secret: String::new(),
        dest: "55553".into(),
        mode_shape: astar_iax::CallMode::Standard,
        permanent: false,
    };
    let v = LinkValidator::new(KnownNodes::from_iter_labels(["55553"]));
    assert_eq!(v.validate("55553"), LinkAdmission::Accepted);
    let _ev = LinkEvent::Connected {
        node: "55553".into(),
        call: 1,
    };
}
