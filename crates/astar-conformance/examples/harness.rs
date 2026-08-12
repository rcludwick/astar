// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! CLI dispatcher for the Asterisk parity harness.
//!
//!     IAX_PEER=127.0.0.1:4569 \
//!     IAX_USER=astartest \
//!     IAX_SECRET=supersecret \
//!     cargo run --example harness -- call_token
//!     cargo run --example harness -- call_dtmf
//!
//! Exit codes: 0 on success, 2 on usage error, 1 on scenario failure.

use std::env;
use std::process::ExitCode;
use std::sync::Arc;

use astar_conformance::driver::Session;
use astar_conformance::scenarios;
use astar_iax_core::session::auth::{AuthMethods, Credentials, Secret};

fn main() -> ExitCode {
    let Some(scenario) = env::args().nth(1) else {
        eprintln!(
            "usage: harness <register|call_notoken|call_token|call_ulaw|peer_hangup|call_dtmf>"
        );
        return ExitCode::from(2);
    };

    let peer: std::net::SocketAddr = env::var("IAX_PEER")
        .unwrap_or_else(|_| "127.0.0.1:4569".into())
        .parse()
        .expect("IAX_PEER not a valid SocketAddr");
    let username = env::var("IAX_USER").unwrap_or_else(|_| default_user_for(&scenario));
    let secret = env::var("IAX_SECRET").unwrap_or_else(|_| "supersecret".into());

    let creds = Credentials {
        username,
        password: Arc::new(Secret::new(secret)),
        allowed_methods: AuthMethods::MD5,
    };

    let mut session = match Session::connect(peer, creds) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect: {e}");
            return ExitCode::from(1);
        }
    };

    match scenarios::dispatch(&scenario, &mut session) {
        Ok(()) => {
            eprintln!("scenario {scenario} ok");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("scenario {scenario} failed: {e:?}");
            ExitCode::from(1)
        }
    }
}

fn default_user_for(scenario: &str) -> String {
    match scenario {
        "call_notoken" => "astartest_notok".into(),
        _ => "astartest".into(),
    }
}
