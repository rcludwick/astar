// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! End-to-end ceremonies through the `/auth/*` handlers, with software
//! authenticators from webauthn-authenticator-rs: `SoftPasskey` (user
//! verification) and `SoftToken` (no PIN). No hardware, no network.

use std::collections::HashMap;

use serde_json::{Value, json};
use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_authenticator_rs::softtoken::SoftToken;
use webauthn_rs::prelude::{
    CreationChallengeResponse, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse, Url,
};

use super::service::tests::Fixture;
use super::service::{AuthService, Req, Surface};
use super::store::{CredentialKind, Role};

pub(crate) const HOST: &str = "node.example.test";

/// A browser: a cookie jar and a host, calling the handlers directly.
pub(crate) struct Browser<'a> {
    pub svc: &'a AuthService,
    pub host: String,
    pub jar: HashMap<String, String>,
    pub tls: bool,
    pub ip: &'static str,
}

impl<'a> Browser<'a> {
    pub(crate) fn new(svc: &'a AuthService) -> Self {
        Self::on(svc, HOST)
    }

    pub(crate) fn on(svc: &'a AuthService, host: &str) -> Self {
        Self {
            svc,
            host: host.to_string(),
            jar: HashMap::new(),
            tls: true,
            ip: "203.0.113.9:40000",
        }
    }

    pub(crate) fn origin(&self) -> Url {
        Url::parse(&format!("https://{}", self.host)).unwrap()
    }

    /// Call a route; the status, the JSON body, and every `Set-Cookie`
    /// applied to the jar.
    pub(crate) fn call(&mut self, method: &str, path: &str, body: &Value) -> (u16, Value) {
        let cookie: String = self
            .jar
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        let bytes = if method == "GET" {
            Vec::new()
        } else {
            serde_json::to_vec(body).unwrap()
        };
        let origin = format!("https://{}", self.host);
        let (path_only, query) = path.split_once('?').unwrap_or((path, ""));
        let req = Req {
            method,
            path: path_only,
            query,
            body: &bytes,
            host: Some(&self.host),
            origin: (method != "GET").then_some(origin.as_str()),
            content_type: Some("application/json"),
            sec_fetch_site: Some("same-origin"),
            cookie: (!cookie.is_empty()).then_some(cookie.as_str()),
            user_agent: Some("SoftBrowser/1.0"),
            peer: Some(self.ip.parse().unwrap()),
            forwarded: false,
            surface: Surface::Public,
            tls: self.tls,
        };
        let resp = super::handle(self.svc, &req).expect("an /auth route");
        for (k, v) in &resp.headers {
            if *k != "Set-Cookie" {
                continue;
            }
            let pair = v.split(';').next().unwrap();
            let (name, value) = pair.split_once('=').unwrap();
            if v.contains("Max-Age=0") {
                self.jar.remove(name);
            } else {
                self.jar.insert(name.to_string(), value.to_string());
            }
        }
        let v = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        (resp.status, v)
    }
}

/// An admin's browser, signed in by the test hook.
pub(crate) fn admin_browser(svc: &AuthService) -> Browser<'_> {
    let mut b = Browser::new(svc);
    let token = svc.test_sign_in("BOSS", Role::Admin);
    b.jar
        .insert(super::session::SESSION_COOKIE_TLS.to_string(), token);
    b
}

/// Make an invite as BOSS and return its token.
pub(crate) fn invite(svc: &AuthService, body: &Value) -> String {
    let mut boss = admin_browser(svc);
    let (st, v) = boss.call("POST", "/auth/invites", body);
    assert_eq!(st, 200, "{v}");
    let link = v["link"].as_str().unwrap();
    assert!(
        link.starts_with(&format!("https://{HOST}/admin/invite#")),
        "{link}"
    );
    link.split_once('#').unwrap().1.to_string()
}

/// Register through an invite with `auth`.
fn redeem<T: webauthn_authenticator_rs::AuthenticatorBackend>(
    b: &mut Browser<'_>,
    auth: &mut WebauthnAuthenticator<T>,
    token: &str,
    name: &str,
    kind: &str,
) -> (u16, Value, Option<RegisterPublicKeyCredential>) {
    let (st, v) = b.call(
        "POST",
        "/auth/invite/begin",
        &json!({ "token": token, "name": name, "kind": kind }),
    );
    if st != 200 {
        return (st, v, None);
    }
    let ccr: CreationChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
    let cred = auth
        .do_registration(b.origin(), ccr)
        .expect("the authenticator registers");
    let (st, v) = b.call(
        "POST",
        "/auth/invite/finish",
        &json!({ "credential": cred }),
    );
    (st, v, Some(cred))
}

/// Identifier-first sign-in.
fn sign_in_by_name<T: webauthn_authenticator_rs::AuthenticatorBackend>(
    b: &mut Browser<'_>,
    auth: &mut WebauthnAuthenticator<T>,
    name: &str,
    pin: Option<bool>,
) -> (u16, Value) {
    let mut req = json!({ "name": name });
    if let Some(p) = pin {
        req["pin"] = json!(p);
    }
    let (st, v) = b.call("POST", "/auth/key/begin", &req);
    assert_eq!(st, 200, "{v}");
    let rcr: RequestChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
    let Ok(cred) = auth.do_authentication(b.origin(), rcr) else {
        return (0, Value::Null);
    };
    b.call("POST", "/auth/key/finish", &json!({ "credential": cred }))
}

/// The discoverable button. The soft authenticators keep no resident
/// keys, so the "browser" here names the credential the user picked, and
/// adds the user handle a resident key would return; neither is signed.
fn sign_in_discoverable<T: webauthn_authenticator_rs::AuthenticatorBackend>(
    b: &mut Browser<'_>,
    auth: &mut WebauthnAuthenticator<T>,
    cred_id: &Value,
    user_handle: &[u8],
) -> (u16, Value) {
    let (st, v) = b.call("POST", "/auth/passkey/begin", &json!({}));
    assert_eq!(st, 200, "{v}");
    assert!(
        v["options"].get("mediation").is_none(),
        "the button is modal: {v}"
    );
    let mut options = v["options"].clone();
    options["publicKey"]["allowCredentials"] = json!([{ "type": "public-key", "id": cred_id }]);
    let rcr: RequestChallengeResponse = serde_json::from_value(options).unwrap();
    let mut cred: PublicKeyCredential = auth.do_authentication(b.origin(), rcr).unwrap();
    cred.response.user_handle = Some(user_handle.to_vec().into());
    b.call(
        "POST",
        "/auth/passkey/finish",
        &json!({ "credential": cred }),
    )
}

fn passkey() -> WebauthnAuthenticator<SoftPasskey> {
    WebauthnAuthenticator::new(SoftPasskey::new(true))
}

fn token_key() -> (WebauthnAuthenticator<SoftToken>, ()) {
    let (t, _) = SoftToken::new(false).unwrap();
    (WebauthnAuthenticator::new(t), ())
}

#[test]
fn invite_register_then_every_way_to_sign_in() {
    let f = Fixture::new();
    let token = invite(&f.svc, &json!({ "role": "admin" }));

    let mut b = Browser::new(&f.svc);
    let (st, info) = b.call("POST", "/auth/invite/info", &json!({ "token": token }));
    assert_eq!(st, 200, "{info}");
    assert_eq!(info["role"], "admin");
    assert_eq!(info["inviter"], "BOSS");

    let mut pk = passkey();
    let (st, v, reg) = redeem(&mut b, &mut pk, &token, "W1ABC", "passkey");
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["name"], "W1ABC");
    let (st, me) = b.call("GET", "/auth/me", &json!({}));
    assert_eq!(
        (st, me["name"].as_str(), me["role"].as_str()),
        (200, Some("W1ABC"), Some("admin"))
    );
    assert_eq!(me["host_has_webauthn"], true);

    // The invite is spent.
    let (st, v) =
        Browser::new(&f.svc).call("POST", "/auth/invite/info", &json!({ "token": token }));
    assert_eq!((st, v["error"].as_str()), (410, Some("invite_used")));

    // Sign out, then the passkey button.
    let (st, _) = b.call("POST", "/auth/signout", &json!({}));
    assert_eq!(st, 200);
    assert_eq!(b.call("GET", "/auth/me", &json!({})).0, 401);
    let user = f.svc.read(|d| d.user_by_name("W1ABC").unwrap().id);
    let raw_id = serde_json::to_value(reg.unwrap().raw_id).unwrap();
    let (st, v) = sign_in_discoverable(&mut b, &mut pk, &raw_id, user.as_bytes());
    assert_eq!(st, 200, "{v}");
    assert_eq!(b.call("GET", "/auth/me", &json!({})).0, 200);

    // And by name.
    let mut fresh = Browser::new(&f.svc);
    let (st, v) = sign_in_by_name(&mut fresh, &mut pk, "w1abc", None);
    assert_eq!(st, 200, "{v}");
    let audit = f
        .svc
        .read(|d| d.audit.iter().map(|e| e.event.clone()).collect::<Vec<_>>());
    for e in [
        "invite_create",
        "invite_use",
        "credential_add",
        "sign_in",
        "sign_out",
    ] {
        assert!(audit.iter().any(|a| a == e), "{e} audited: {audit:?}");
    }
}

#[test]
fn a_security_key_without_a_pin_registers_and_signs_in() {
    let f = Fixture::new();
    let token = invite(&f.svc, &json!({ "role": "viewer" }));
    let mut b = Browser::new(&f.svc);
    let (mut key, ()) = token_key();
    let (st, v, _) = redeem(&mut b, &mut key, &token, "K2KEY", "security_key");
    assert_eq!(st, 200, "{v}");
    let kinds = f.svc.read(|d| {
        d.credentials
            .iter()
            .map(|c| c.kind.name())
            .collect::<Vec<_>>()
    });
    assert_eq!(kinds, ["security_key"]);
    let mut b = Browser::new(&f.svc);
    let (st, v) = b.call("POST", "/auth/key/begin", &json!({ "name": "K2KEY" }));
    assert_eq!((st, v["no_pin_keys"].as_bool()), (200, Some(true)));
    let (st, v) = sign_in_by_name(&mut b, &mut key, "K2KEY", Some(false));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["role"], "viewer");
}

#[test]
fn a_cloned_key_with_a_rolled_back_counter_is_refused_and_audited() {
    use webauthn_authenticator_rs::softtoken::SoftTokenFile;
    let f = Fixture::new();
    let token = invite(&f.svc, &json!({ "role": "admin" }));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token.cbor");
    let open = |p: &std::path::Path| {
        std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(p)
            .unwrap()
    };
    {
        let (t, _) = SoftToken::new(false).unwrap();
        let mut key = WebauthnAuthenticator::new(SoftTokenFile::new(t, open(&path)));
        let (st, v, _) = redeem(
            &mut Browser::new(&f.svc),
            &mut key,
            &token,
            "CL0NE",
            "security_key",
        );
        assert_eq!(st, 200, "{v}");
    } // saved on drop
    // A copy of the key taken now, before its counter moves.
    let copy = dir.path().join("copy.cbor");
    std::fs::copy(&path, &copy).unwrap();
    {
        let mut key = WebauthnAuthenticator::new(SoftTokenFile::open(open(&path)).unwrap());
        for _ in 0..2 {
            let (st, v) = sign_in_by_name(&mut Browser::new(&f.svc), &mut key, "CL0NE", None);
            assert_eq!(st, 200, "{v}");
        }
    }
    let mut clone = WebauthnAuthenticator::new(SoftTokenFile::open(open(&copy)).unwrap());
    let (st, v) = sign_in_by_name(&mut Browser::new(&f.svc), &mut clone, "CL0NE", None);
    assert_eq!(
        (st, v["error"].as_str()),
        (401, Some("counter_regression")),
        "{v}"
    );
    let audited = f
        .svc
        .read(|d| d.audit.iter().any(|e| e.event == "counter_regression"));
    assert!(audited);
}

#[test]
fn a_credential_for_one_host_is_refused_on_another() {
    let f = Fixture::with(|s| {
        s.public_hostnames = vec![HOST.to_string(), "other.example.test".to_string()];
    });
    let token = invite(&f.svc, &json!({ "role": "admin" }));
    let mut b = Browser::new(&f.svc);
    let mut pk = passkey();
    let (st, _, reg) = redeem(&mut b, &mut pk, &token, "W1ABC", "passkey");
    assert_eq!(st, 200);
    let raw_id = serde_json::to_value(reg.unwrap().raw_id).unwrap();
    let user = f.svc.read(|d| d.user_by_name("W1ABC").unwrap().id);

    let mut elsewhere = Browser::on(&f.svc, "other.example.test");
    let (st, v) = elsewhere.call("POST", "/auth/key/begin", &json!({ "name": "W1ABC" }));
    assert_eq!(st, 200);
    let offered: Vec<Value> = v["options"]["publicKey"]["allowCredentials"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].clone())
        .collect();
    assert!(
        !offered.contains(&raw_id),
        "host B never offers host A's key"
    );
    let (st, v) = sign_in_discoverable(&mut elsewhere, &mut pk, &raw_id, user.as_bytes());
    assert_eq!(
        (st, v["error"].as_str()),
        (401, Some("unknown_credential")),
        "{v}"
    );
}

#[test]
fn an_unknown_name_gets_an_answer_of_the_same_shape() {
    let f = Fixture::new();
    let mut b = Browser::new(&f.svc);
    let mut any = false;
    for name in ["NOBODY", "N0BODY", "W9XYZ", "SOMEONE", "K1AAA"] {
        let (st, v) = b.call("POST", "/auth/key/begin", &json!({ "name": name }));
        assert_eq!(st, 200, "{v}");
        let ids = v["options"]["publicKey"]["allowCredentials"].clone();
        assert!(ids.is_array(), "{v}");
        any |= !ids.as_array().unwrap().is_empty();
        let (_, again) = b.call(
            "POST",
            "/auth/key/begin",
            &json!({ "name": name.to_ascii_lowercase() }),
        );
        assert_eq!(
            again["options"]["publicKey"]["allowCredentials"], ids,
            "the same name gets the same ids"
        );
    }
    assert!(any, "realistic made-up ids for most names");
    let (st, v) = b.call("POST", "/auth/key/finish", &json!({ "credential": {
        "id": "AAAA", "rawId": "AAAA", "type": "public-key",
        "response": { "authenticatorData": "AAAA", "clientDataJSON": "AAAA", "signature": "AAAA" }
    }}));
    assert_eq!(
        (st, v["error"].as_str()),
        (401, Some("unknown_credential")),
        "{v}"
    );
}

#[test]
fn the_exclude_list_names_the_users_keys_and_re_enrol_attaches_to_them() {
    let f = Fixture::new();
    let token = invite(&f.svc, &json!({ "role": "admin" }));
    let mut b = Browser::new(&f.svc);
    let mut first = passkey();
    let (st, _, reg) = redeem(&mut b, &mut first, &token, "W1ABC", "passkey");
    assert_eq!(st, 200);
    let raw_id = serde_json::to_value(reg.unwrap().raw_id).unwrap();
    let user = f.svc.read(|d| d.user_by_name("W1ABC").unwrap().id);

    let token = invite(&f.svc, &json!({ "user_id": user.to_string() }));
    let mut b = Browser::new(&f.svc);
    let (st, v) = b.call(
        "POST",
        "/auth/invite/begin",
        &json!({ "token": token, "name": "IGNORED", "kind": "passkey" }),
    );
    assert_eq!(st, 200, "{v}");
    let excluded: Vec<Value> = v["options"]["publicKey"]["excludeCredentials"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].clone())
        .collect();
    assert!(excluded.contains(&raw_id), "{v}");
    // A second device enrols for the same user.
    let mut second = passkey();
    let (st, v, _) = redeem(
        &mut Browser::new(&f.svc),
        &mut second,
        &token,
        "IGNORED",
        "passkey",
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["name"], "W1ABC");
    let (users, creds) = f
        .svc
        .read(|d| (d.users.len(), d.credentials_of(user).count()));
    assert_eq!((users, creds), (2, 2), "BOSS and W1ABC; two passkeys");
}

#[test]
fn an_abandoned_ceremony_leaves_the_invite_usable() {
    let f = Fixture::new();
    let token = invite(&f.svc, &json!({ "role": "viewer" }));
    let mut b = Browser::new(&f.svc);
    let (st, _) = b.call(
        "POST",
        "/auth/invite/begin",
        &json!({ "token": token, "name": "LEFT", "kind": "passkey" }),
    );
    assert_eq!(st, 200);
    // Walked away. A later attempt works.
    let (st, v, _) = redeem(
        &mut Browser::new(&f.svc),
        &mut passkey(),
        &token,
        "LATER",
        "passkey",
    );
    assert_eq!(st, 200, "{v}");
}

#[test]
fn two_redemptions_at_once_make_one_user() {
    let f = Fixture::new();
    let token = invite(&f.svc, &json!({ "role": "admin" }));
    let mut a = Browser::new(&f.svc);
    let mut b = Browser::new(&f.svc);
    let (mut pa, mut pb) = (passkey(), passkey());
    let begin = |br: &mut Browser<'_>, name: &str| {
        let (st, v) = br.call(
            "POST",
            "/auth/invite/begin",
            &json!({ "token": token, "name": name, "kind": "passkey" }),
        );
        assert_eq!(st, 200, "{v}");
        serde_json::from_value::<CreationChallengeResponse>(v["options"].clone()).unwrap()
    };
    let ca = begin(&mut a, "FIRST");
    let cb = begin(&mut b, "SECOND");
    let ra = pa.do_registration(a.origin(), ca).unwrap();
    let rb = pb.do_registration(b.origin(), cb).unwrap();
    let (sa, _) = a.call("POST", "/auth/invite/finish", &json!({ "credential": ra }));
    let (sb, vb) = b.call("POST", "/auth/invite/finish", &json!({ "credential": rb }));
    assert_eq!(sa, 200);
    assert_eq!((sb, vb["error"].as_str()), (410, Some("invite_used")));
    assert_eq!(f.svc.read(|d| d.users.len()), 2, "BOSS and FIRST");
}

#[test]
fn invite_tokens_are_stored_hashed_and_expire() {
    let f = Fixture::new();
    let token = invite(&f.svc, &json!({ "role": "viewer", "hours": 1 }));
    let on_disk =
        std::fs::read_to_string(super::store::store_path(&f.svc.settings.state_dir)).unwrap();
    assert!(!on_disk.contains(&token));
    assert!(on_disk.contains(&super::session::hash_token(&token)));
    f.advance(3601);
    let (st, v) =
        Browser::new(&f.svc).call("POST", "/auth/invite/info", &json!({ "token": token }));
    assert_eq!((st, v["error"].as_str()), (410, Some("invite_expired")));
}

#[test]
fn a_revoked_invite_is_dead_and_only_admins_with_step_up_make_them() {
    let f = Fixture::new();
    let token = invite(&f.svc, &json!({ "role": "viewer" }));
    let mut boss = admin_browser(&f.svc);
    let (_, list) = boss.call("GET", "/auth/invites", &json!({}));
    let id = list["invites"][0]["id"].as_str().unwrap().to_string();
    assert!(
        list.to_string().find(&token).is_none(),
        "the list never shows a token"
    );
    let (st, _) = boss.call("DELETE", &format!("/auth/invites/{id}"), &json!({}));
    assert_eq!(st, 200);
    let (st, v) =
        Browser::new(&f.svc).call("POST", "/auth/invite/info", &json!({ "token": token }));
    assert_eq!((st, v["error"].as_str()), (410, Some("invite_revoked")));

    let mut viewer = Browser::new(&f.svc);
    viewer.jar.insert(
        super::session::SESSION_COOKIE_TLS.to_string(),
        f.svc.test_sign_in("V1EW", Role::Viewer),
    );
    assert_eq!(
        viewer
            .call("POST", "/auth/invites", &json!({ "role": "admin" }))
            .0,
        403
    );
    f.advance(f.svc.settings.step_up_secs + 1);
    let (st, v) = boss.call("POST", "/auth/invites", &json!({ "role": "admin" }));
    assert_eq!((st, v["error"].as_str()), (401, Some("reauth_required")));
}

#[test]
fn the_nickname_comes_from_the_authenticator_or_the_kind() {
    let f = Fixture::new();
    let token = invite(&f.svc, &json!({ "role": "viewer" }));
    let (st, _, _) = redeem(
        &mut Browser::new(&f.svc),
        &mut passkey(),
        &token,
        "NICK",
        "passkey",
    );
    assert_eq!(st, 200);
    let (nick, rp) = f.svc.read(|d| {
        let c = &d.credentials[0];
        (c.nickname.clone(), c.kind.rp_id().map(str::to_string))
    });
    assert_eq!(nick, "Passkey", "a soft authenticator has no known AAGUID");
    assert_eq!(rp.as_deref(), Some(HOST));
    assert!(
        f.svc
            .read(|d| matches!(d.credentials[0].kind, CredentialKind::Passkey { .. }))
    );
}

#[test]
fn passkeys_ask_to_be_discoverable_and_keys_with_a_pin_do_not() {
    let f = Fixture::new();
    let token = invite(&f.svc, &json!({ "role": "viewer" }));
    let mut b = Browser::new(&f.svc);
    let rk = |b: &mut Browser<'_>, kind: &str| {
        let (st, v) = b.call(
            "POST",
            "/auth/invite/begin",
            &json!({ "token": token, "name": "RK", "kind": kind }),
        );
        assert_eq!(st, 200, "{v}");
        v["options"]["publicKey"]["authenticatorSelection"]["residentKey"].clone()
    };
    assert_eq!(rk(&mut b, "passkey"), "preferred");
    assert_eq!(rk(&mut b, "pin_key"), "discouraged");
    assert_eq!(rk(&mut b, "security_key"), "discouraged");
}

/// An identifier-first answer with what changes on every call taken out:
/// the challenge, the credential ids and how many, and transports (a
/// soft token's are not a real key's).
fn shape_of(v: &Value) -> Value {
    let mut v = v.clone();
    v["options"]["publicKey"]["challenge"] = json!("*");
    let allow = v["options"]["publicKey"]["allowCredentials"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(!allow.is_empty(), "some credentials are offered: {v}");
    let mut first = allow[0].clone();
    first["id"] = json!("*");
    if let Some(o) = first.as_object_mut() {
        o.remove("transports");
    }
    v["options"]["publicKey"]["allowCredentials"] = json!([first]);
    v
}

fn key_begin(f: &Fixture, name: &str, pin: Option<bool>) -> Value {
    // Past the per-minute limit on begins.
    f.advance(4);
    let svc = &f.svc;
    let mut req = json!({ "name": name });
    if let Some(p) = pin {
        req["pin"] = json!(p);
    }
    let (st, v) = Browser::new(svc).call("POST", "/auth/key/begin", &req);
    assert_eq!(st, 200, "{v}");
    v
}

/// An unknown name the decoy answers for as if it were `persona`.
fn decoy_named(svc: &AuthService, persona: super::flows::DecoyPersona) -> String {
    (0..10_000)
        .map(|i| format!("N0NE{i}"))
        .find(|n| super::flows::decoy_persona(svc, n) == persona)
        .expect("every persona turns up")
}

#[test]
fn every_unknown_name_is_offered_some_credentials() {
    let f = Fixture::new();
    for i in 0..300 {
        let name = format!("N0NE{i}");
        for pin in [None, Some(false)] {
            let v = key_begin(&f, &name, pin);
            assert!(
                !v["options"]["publicKey"]["allowCredentials"]
                    .as_array()
                    .unwrap()
                    .is_empty(),
                "{name}: {v}"
            );
        }
    }
}

#[test]
fn an_unknown_name_answers_like_a_real_one_of_some_kind() {
    use super::flows::DecoyPersona;
    let f = Fixture::new();
    let t = invite(&f.svc, &json!({ "role": "viewer" }));
    assert_eq!(
        redeem(
            &mut Browser::new(&f.svc),
            &mut passkey(),
            &t,
            "PK",
            "passkey"
        )
        .0,
        200
    );
    let t = invite(&f.svc, &json!({ "role": "viewer" }));
    let mut tok = WebauthnAuthenticator::new(SoftToken::new(false).unwrap().0);
    assert_eq!(
        redeem(
            &mut Browser::new(&f.svc),
            &mut tok,
            &t,
            "SK",
            "security_key"
        )
        .0,
        200
    );
    // BOTH: a passkey and a key without a PIN.
    let t = invite(&f.svc, &json!({ "role": "viewer" }));
    assert_eq!(
        redeem(
            &mut Browser::new(&f.svc),
            &mut passkey(),
            &t,
            "BOTH",
            "passkey"
        )
        .0,
        200
    );
    let t = invite(
        &f.svc,
        &json!({ "user_id": f.svc.read(|d| d.user_by_name("BOTH").unwrap().id.to_string()) }),
    );
    let mut tok2 = WebauthnAuthenticator::new(SoftToken::new(false).unwrap().0);
    assert_eq!(
        redeem(
            &mut Browser::new(&f.svc),
            &mut tok2,
            &t,
            "BOTH",
            "security_key"
        )
        .0,
        200
    );

    for (real, persona) in [
        ("PK", DecoyPersona::Passkeys),
        ("SK", DecoyPersona::Keys),
        ("BOTH", DecoyPersona::Both),
    ] {
        let nobody = decoy_named(&f.svc, persona);
        for pin in [None, Some(true), Some(false)] {
            let r = key_begin(&f, real, pin);
            let d = key_begin(&f, &nobody, pin);
            assert_eq!(
                shape_of(&r),
                shape_of(&d),
                "{real} vs {nobody}, pin {pin:?}"
            );
            // And the same unknown name gets the same made-up answer.
            let again = key_begin(&f, &nobody, pin);
            assert_eq!(
                d["options"]["publicKey"]["allowCredentials"],
                again["options"]["publicKey"]["allowCredentials"]
            );
        }
    }
}

#[test]
fn rotating_addresses_within_one_ipv6_64_share_its_limits() {
    let f = Fixture::new();
    let mut last = 0;
    for i in 0..=super::limits::BEGINS_PER_MINUTE {
        let mut b = Browser::new(&f.svc);
        b.ip = Box::leak(format!("[2001:db8:5:6::{:x}]:4000", i + 1).into_boxed_str());
        last = b.call("POST", "/auth/passkey/begin", &json!({})).0;
    }
    assert_eq!(last, 429, "a fresh /128 is not a fresh client");
    let mut other = Browser::new(&f.svc);
    other.ip = "[2001:db8:5:7::1]:4000";
    assert_eq!(
        other.call("POST", "/auth/passkey/begin", &json!({})).0,
        200,
        "the next /64 is"
    );
}
