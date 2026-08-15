// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! End-to-end ASL3 call recipe — the copy-paste integration example
//! (iax-53da). Mint a Web Transceiver token, resolve the node, connect a
//! `ConsoleSession`, key PTT (optionally from a UCI150 handset), print state.
//!
//! Examples may read env (libraries may not):
//!   `ASL_USER`=<callsign> `ASL_PASS`=<portal password> `ASL_NODE`=<your node>
//!   NODE=55553 cargo run -p astar-console --example `asl3_call`
//!
//! Press Enter to toggle PTT; type `q` + Enter to hang up.

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astar_audio::CpalBackend;
use astar_console::{ConsoleConfig, ConsoleSession};
use astar_iax::CodecPolicy;

fn env(k: &str) -> Option<String> {
    std::env::var(k)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn main() {
    let node = env("NODE").unwrap_or_else(|| "55553".into());
    let creds = astar_asl3::PortalCredentials {
        user: env("ASL_USER").expect("set ASL_USER"),
        password: env("ASL_PASS").expect("set ASL_PASS"),
        node: env("ASL_NODE").expect("set ASL_NODE (a node you own)"),
    };

    // 1. ASL3 services: token + address.
    let token = astar_asl3::mint_wt_token(&creds).expect("mint WT token");
    let peer = astar_asl3::resolve_node(&node).expect("resolve node");
    println!("node {node} at {peer}; token minted");

    // 2. Connect (Web Transceiver shape: token as CALLING_NAME, dest node as
    //    CALLING_NUMBER, guest secret).
    let session = Arc::new(Mutex::new(ConsoleSession::new()));
    session
        .lock()
        .unwrap()
        .connect(
            Box::new(CpalBackend::new()),
            peer,
            ConsoleConfig {
                node: node.clone(),
                calling_node: node.clone(),
                secret: "allstar".into(),
                name: token,
                input_device: env("INPUT"),
                output_device: env("OUTPUT"),
                codec_policy: CodecPolicy::default(),
            },
        )
        .expect("connect");

    // 3. Optional hardware PTT: UCI150 handset, if present.
    let ptt_stop = Arc::new(AtomicBool::new(false));
    let _ptt = astar_ptt::Uci150Serial::autodetect()
        .and_then(|path| astar_ptt::Uci150Serial::open(&path).ok())
        .map(|backend| {
            let s = Arc::clone(&session);
            let s2 = Arc::clone(&session);
            astar_ptt::spawn(
                Box::new(backend),
                astar_ptt::BridgeConfig {
                    cts_keyed_high: true,
                    rts_key_high: true,
                    cts_debounce: Duration::from_millis(30),
                    rx_mode: astar_ptt::RxKeyMode::RemotePtt,
                    rx_floor_db: -45.0,
                    rx_hang: Duration::from_millis(250),
                },
                astar_ptt::PttIo {
                    on_key: Box::new(move |on| {
                        let _ = s.lock().unwrap().set_ptt(on);
                    }),
                    radio_input: Box::new(move || {
                        let snap = s2.lock().unwrap().snapshot();
                        astar_ptt::RadioKeyInput {
                            remote_keyed: snap.remote_ptt,
                            rx_level_db: snap.rx_level_db,
                        }
                    }),
                },
                Arc::clone(&ptt_stop),
            )
        });

    // 4. Drive: poll state in the background, toggle PTT from stdin.
    let poll_session = Arc::clone(&session);
    let stop = Arc::new(AtomicBool::new(false));
    let poll_stop = Arc::clone(&stop);
    let poller = std::thread::spawn(move || {
        while !poll_stop.load(Ordering::Relaxed) {
            let snap = poll_session.lock().unwrap().snapshot();
            println!(
                "status={:?} ptt={} remote_ptt={} tx={:.0}dB rx={:.0}dB",
                snap.status, snap.ptt, snap.remote_ptt, snap.tx_level_db, snap.rx_level_db
            );
            std::thread::sleep(Duration::from_secs(1));
        }
    });

    let stdin = std::io::stdin();
    let mut keyed = false;
    for line in stdin.lock().lines() {
        let line = line.unwrap_or_default();
        if line.trim() == "q" {
            break;
        }
        keyed = !keyed;
        let _ = session.lock().unwrap().set_ptt(keyed);
        println!("PTT {}", if keyed { "KEYED" } else { "released" });
    }

    stop.store(true, Ordering::Relaxed);
    ptt_stop.store(true, Ordering::Relaxed);
    let _ = session.lock().unwrap().disconnect();
    let _ = poller.join();
}
