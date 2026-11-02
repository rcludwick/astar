// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! No secret reaches a log line, the audit, the store or a response that is
//! not the one whose purpose is to hand it over.
//!
//! Every flow runs under a capturing `tracing` subscriber at TRACE, the
//! loudest the node or webauthn-rs can be. Then every session cookie,
//! ceremony id, invite token, TOTP secret and code seen along the way is
//! searched for in all of it.

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::json;
use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse};

use super::service::tests::Fixture;
use super::tests_e2e::{Browser, admin_browser};
use super::tests_totp::code;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Every secret a run saw, and every response body that is not allowed to
/// carry one.
#[derive(Default)]
struct Seen {
    secrets: Vec<String>,
    bodies: Vec<String>,
    /// The one secret auth.json must keep: HMAC needs it.
    totp_secret: String,
}

impl Seen {
    fn jar(&mut self, b: &Browser<'_>) {
        for v in b.jar.values() {
            self.secrets.push(v.clone());
        }
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end run, read top to bottom"
)]
fn no_secret_reaches_a_log_the_audit_the_store_or_a_bystander_response() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(capture.clone())
        .with_ansi(false)
        .finish();
    let f = Fixture::new();
    let mut seen = Seen::default();

    tracing::subscriber::with_default(subscriber, || {
        // Callsites that fired before this test, with no subscriber, cached
        // "never interested"; ask them again now there is one.
        tracing::callsite::rebuild_interest_cache();
        tracing::info!("capture-marker");
        // An invite: the creating response carries the link (its purpose).
        let mut boss = admin_browser(&f.svc);
        seen.jar(&boss);
        let (_, v) = boss.call("POST", "/auth/invites", &json!({ "role": "admin" }));
        let token = v["link"]
            .as_str()
            .unwrap()
            .split_once('#')
            .unwrap()
            .1
            .to_string();
        seen.secrets.push(token.clone());

        // Redeem with a passkey; every other response is a bystander.
        let mut b = Browser::new(&f.svc);
        let mut pk = WebauthnAuthenticator::new(SoftPasskey::new(true));
        let (_, info) = b.call("POST", "/auth/invite/info", &json!({ "token": token }));
        seen.bodies.push(info.to_string());
        let (_, v) = b.call(
            "POST",
            "/auth/invite/begin",
            &json!({ "token": token, "name": "W1ABC", "kind": "passkey" }),
        );
        seen.bodies.push(v.to_string());
        seen.jar(&b);
        let ccr: CreationChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
        let reg = pk.do_registration(b.origin(), ccr).unwrap();
        let (_, v) = b.call("POST", "/auth/invite/finish", &json!({ "credential": reg }));
        seen.bodies.push(v.to_string());
        seen.jar(&b);

        // Add an app: the begin response carries the secret (its purpose).
        let (_, v) = b.call("POST", "/auth/credentials/totp/begin", &json!({}));
        let secret = v["secret"].as_str().unwrap().to_string();
        seen.secrets.push(secret.clone());
        seen.totp_secret.clone_from(&secret);
        seen.jar(&b);
        let c1 = code(&secret, f.svc.now());
        seen.secrets.push(c1.clone());
        let (_, v) = b.call(
            "POST",
            "/auth/credentials/totp/finish",
            &json!({ "code": c1 }),
        );
        seen.bodies.push(v.to_string());

        // A wrong code, a right one, a step-up, and every list.
        let mut c = Browser::new(&f.svc);
        let (_, v) = c.call(
            "POST",
            "/auth/totp",
            &json!({ "name": "W1ABC", "code": "000000" }),
        );
        seen.bodies.push(v.to_string());
        f.advance(30);
        let c2 = code(&secret, f.svc.now());
        seen.secrets.push(c2.clone());
        let (_, v) = c.call(
            "POST",
            "/auth/totp",
            &json!({ "name": "W1ABC", "code": c2 }),
        );
        seen.bodies.push(v.to_string());
        seen.jar(&c);
        f.advance(30);
        let c3 = code(&secret, f.svc.now());
        seen.secrets.push(c3.clone());
        let (_, v) = c.call("POST", "/auth/stepup/totp", &json!({ "code": c3 }));
        seen.bodies.push(v.to_string());
        let (_, v) = c.call("POST", "/auth/key/begin", &json!({ "name": "W1ABC" }));
        seen.bodies.push(v.to_string());
        seen.jar(&c);
        let rcr: RequestChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
        let cred = pk.do_authentication(c.origin(), rcr).unwrap();
        let (_, v) = c.call("POST", "/auth/key/finish", &json!({ "credential": cred }));
        seen.bodies.push(v.to_string());
        seen.jar(&c);
        for path in [
            "/auth/me",
            "/auth/credentials",
            "/auth/sessions",
            "/auth/status",
        ] {
            seen.bodies
                .push(c.call("GET", path, &json!({})).1.to_string());
        }
        for path in ["/auth/users", "/auth/invites", "/auth/audit"] {
            seen.bodies
                .push(boss.call("GET", path, &json!({})).1.to_string());
        }
        seen.bodies
            .push(c.call("POST", "/auth/signout", &json!({})).1.to_string());
    });

    let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let audit = f
        .svc
        .read(|d| serde_json::to_string(&d.audit).unwrap_or_default());
    let store =
        std::fs::read_to_string(super::store::store_path(&f.svc.settings.state_dir)).unwrap();
    // The store keeps the TOTP secret (HMAC needs it; the file's mode is
    // the protection) and nothing else on this list.
    assert!(seen.secrets.len() > 8, "the run saw its secrets");
    for s in &seen.secrets {
        assert!(s.len() >= 6, "a real value: {s:?}");
        assert!(!log.contains(s.as_str()), "a secret in the log");
        assert!(!audit.contains(s.as_str()), "a secret in the audit");
        for b in &seen.bodies {
            assert!(
                !b.contains(s.as_str()),
                "a secret in a response that is not for it: {b}"
            );
        }
        if *s != seen.totp_secret {
            assert!(!store.contains(s.as_str()), "a secret in auth.json");
        }
    }
    assert!(
        !log.is_empty(),
        "the subscriber did capture: webauthn-rs logs at debug/trace"
    );
}
