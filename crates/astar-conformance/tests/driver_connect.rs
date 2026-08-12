// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `Session::connect` opens a bound UDP socket and constructs FSM +
//! Reliability without doing any network I/O. State is `Init`.

use std::sync::Arc;

use astar_conformance::driver::Session;
use astar_iax_core::session::auth::{AuthMethods, Credentials, Secret};
use astar_iax_core::session::fsm::SessionState;

fn test_creds() -> Credentials {
    Credentials {
        username: "astartest".into(),
        password: Arc::new(Secret::new("supersecret".into())),
        allowed_methods: AuthMethods::MD5,
    }
}

#[test]
fn connect_constructs_session_in_init_state() {
    let peer = "127.0.0.1:65535".parse().unwrap();
    let session = Session::connect(peer, test_creds()).expect("connect");
    assert!(matches!(session.state(), SessionState::Init));
}
