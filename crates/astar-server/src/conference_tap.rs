// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The conference tap: how a non-contributing member is enrolled, checked
//! and withdrawn. Shared by the listener stream (`stream`) and the node
//! logger (`recording`), so it carries no feature gate of its own.

/// Where the tap gets its member. `NodeController` in the daemon; fakes in
/// tests.
pub trait TapSource: Send + Sync {
    fn enrol(&self) -> Option<astar_iax::BridgeMemberHandle>;
    fn still_enrolled(&self, id: astar_audio::MemberId) -> bool;
    fn withdraw(&self, id: astar_audio::MemberId);
}

impl TapSource for crate::controller::NodeController {
    fn enrol(&self) -> Option<astar_iax::BridgeMemberHandle> {
        self.enrol_stream_tap()
    }
    fn still_enrolled(&self, id: astar_audio::MemberId) -> bool {
        self.stream_tap_enrolled(id)
    }
    fn withdraw(&self, id: astar_audio::MemberId) {
        self.withdraw_stream_tap(id);
    }
}
