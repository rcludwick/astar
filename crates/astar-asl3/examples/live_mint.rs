// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Live diagnostic: mint a Web Transceiver token against the REAL portal —
//! the Rust counterpart of scripts/asl-wt-token.py. Examples may read env
//! (the library itself never does):
//!
//! ```text
//! ASL_USER=<callsign> ASL_PASS=<portal password> ASL_NODE=<your node> \
//!   cargo run -p astar-asl3 --example live_mint
//! ```
//!
//! `ASL_NODE` is optional: leave it unset to mint the way the app does when
//! no node is saved (no `?node=` on the transceiver page), set it to compare.
//!
//! Prints the minted token on stdout (a per-session credential — treat like
//! the Python script's output). On failure the error names its category —
//! login refused, no token in the page, or transport — and nothing else.

fn env(k: &str) -> String {
    std::env::var(k)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            eprintln!("set {k}");
            std::process::exit(2);
        })
}

fn main() {
    let creds = astar_asl3::PortalCredentials {
        user: env("ASL_USER"),
        password: env("ASL_PASS"),
        node: std::env::var("ASL_NODE")
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
    };
    eprintln!(
        "minting for {} {}",
        creds.user,
        if creds.node.is_empty() {
            "with no node".to_string()
        } else {
            format!("with node {}", creds.node)
        }
    );
    match astar_asl3::mint_wt_token(&creds) {
        Ok(token) => println!("{token}"),
        Err(e) => {
            eprintln!("mint failed: {e}");
            std::process::exit(1);
        }
    }
}
