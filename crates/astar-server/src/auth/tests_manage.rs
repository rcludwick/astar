// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Step-up, credentials, sessions, users and the audit, through the
//! handlers with software authenticators.

use serde_json::{Value, json};
use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse};

use super::service::tests::Fixture;
use super::service::{AuthService, Req, Surface};
use super::tests_e2e::{Browser, admin_browser, invite};
use super::tests_totp::code;

type Pk = WebauthnAuthenticator<SoftPasskey>;

/// A user enrolled with a passkey through an invite, signed in.
fn passkey_user<'a>(svc: &'a AuthService, name: &str, role: &str) -> (Browser<'a>, Pk) {
    let token = invite(svc, &json!({ "role": role }));
    let mut b = Browser::new(svc);
    let mut pk = WebauthnAuthenticator::new(SoftPasskey::new(true));
    let (st, v) = b.call(
        "POST",
        "/auth/invite/begin",
        &json!({ "token": token, "name": name, "kind": "passkey" }),
    );
    assert_eq!(st, 200, "{v}");
    let ccr: CreationChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
    let reg = pk.do_registration(b.origin(), ccr).unwrap();
    let (st, v) = b.call("POST", "/auth/invite/finish", &json!({ "credential": reg }));
    assert_eq!(st, 200, "{v}");
    (b, pk)
}

fn step_up_with(b: &mut Browser<'_>, pk: &mut Pk) {
    let (st, v) = b.call("POST", "/auth/stepup/begin", &json!({}));
    assert_eq!(st, 200, "{v}");
    let rcr: RequestChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
    let cred = pk.do_authentication(b.origin(), rcr).unwrap();
    let (st, v) = b.call(
        "POST",
        "/auth/stepup/finish",
        &json!({ "credential": cred }),
    );
    assert_eq!(st, 200, "{v}");
}

fn add_passkey(b: &mut Browser<'_>, pk: &mut Pk, nickname: &str) -> (u16, Value) {
    let (st, v) = b.call("POST", "/auth/credentials/webauthn/begin", &json!({}));
    if st != 200 {
        return (st, v);
    }
    let ccr: CreationChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
    let reg = pk.do_registration(b.origin(), ccr).unwrap();
    b.call(
        "POST",
        "/auth/credentials/webauthn/finish",
        &json!({ "credential": reg, "nickname": nickname }),
    )
}

#[test]
fn adding_a_credential_needs_step_up_and_step_up_works() {
    let f = Fixture::new();
    let (mut b, mut pk) = passkey_user(&f.svc, "W1ABC", "admin");
    f.advance(f.svc.settings.step_up_secs + 1);
    let mut laptop = WebauthnAuthenticator::new(SoftPasskey::new(true));
    let (st, v) = add_passkey(&mut b, &mut laptop, "Laptop");
    assert_eq!((st, v["error"].as_str()), (401, Some("reauth_required")));
    let (_, me) = b.call("GET", "/auth/me", &json!({}));
    assert_eq!(me["stepped_up"], false);
    step_up_with(&mut b, &mut pk);
    let (st, v) = add_passkey(&mut b, &mut laptop, "Laptop");
    assert_eq!(st, 200, "{v}");
    let (_, list) = b.call("GET", "/auth/credentials", &json!({}));
    let nicks: Vec<&str> = list["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["nickname"].as_str().unwrap())
        .collect();
    assert_eq!(nicks, ["Passkey", "Laptop"]);
    assert!(list["credentials"][0]["works_here"].as_bool().unwrap());
    let audit = f
        .svc
        .read(|d| d.audit.iter().filter(|e| e.event == "step_up").count());
    assert_eq!(audit, 1);
}

#[test]
fn step_up_with_an_authenticator_app() {
    let f = Fixture::new();
    let (mut b, _) = passkey_user(&f.svc, "W1ABC", "admin");
    let (_, v) = b.call("POST", "/auth/credentials/totp/begin", &json!({}));
    let secret = v["secret"].as_str().unwrap().to_string();
    let (st, _) = b.call(
        "POST",
        "/auth/credentials/totp/finish",
        &json!({ "code": code(&secret, f.svc.now()) }),
    );
    assert_eq!(st, 200);
    f.advance(f.svc.settings.step_up_secs + 30);
    assert_eq!(b.call("POST", "/auth/reenrol", &json!({})).0, 401);
    let (st, v) = b.call(
        "POST",
        "/auth/stepup/totp",
        &json!({ "code": code(&secret, f.svc.now()) }),
    );
    assert_eq!(st, 200, "{v}");
    let (st, v) = b.call("POST", "/auth/reenrol", &json!({}));
    assert_eq!(st, 200, "{v}");
    assert!(v["link"].as_str().unwrap().contains("/admin/invite#"));
}

#[test]
fn rename_and_remove_with_the_last_credential_kept() {
    let f = Fixture::new();
    let (mut b, _) = passkey_user(&f.svc, "W1ABC", "admin");
    let (_, list) = b.call("GET", "/auth/credentials", &json!({}));
    let id = list["credentials"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(list["credentials"][0]["last"], true);
    let (st, _) = b.call(
        "POST",
        &format!("/auth/credentials/{id}/rename"),
        &json!({ "nickname": "  My <b>phone</b>\u{7} " }),
    );
    assert_eq!(st, 200);
    let nick = f.svc.read(|d| d.credentials[0].nickname.clone());
    assert_eq!(
        nick, "My <b>phone</b>",
        "stored as text; the page never parses it as markup"
    );
    let (st, v) = b.call("DELETE", &format!("/auth/credentials/{id}"), &json!({}));
    assert_eq!((st, v["error"].as_str()), (409, Some("last_credential")));
    let mut second = WebauthnAuthenticator::new(SoftPasskey::new(true));
    assert_eq!(add_passkey(&mut b, &mut second, "Spare").0, 200);
    let (st, v) = b.call("DELETE", &format!("/auth/credentials/{id}"), &json!({}));
    assert_eq!(st, 200, "{v}");
    assert!(
        v["removed"]["webauthn_id"].is_string(),
        "for signalUnknownCredential: {v}"
    );
}

#[test]
fn sessions_are_listed_and_revoked() {
    let f = Fixture::new();
    let (mut b, mut pk) = passkey_user(&f.svc, "W1ABC", "viewer");
    // A second browser.
    let mut other = Browser::new(&f.svc);
    let (_, v) = other.call("POST", "/auth/key/begin", &json!({ "name": "W1ABC" }));
    let rcr: RequestChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
    let cred = pk.do_authentication(other.origin(), rcr).unwrap();
    assert_eq!(
        other
            .call("POST", "/auth/key/finish", &json!({ "credential": cred }))
            .0,
        200
    );
    let (_, list) = b.call("GET", "/auth/sessions", &json!({}));
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions.iter().filter(|s| s["current"] == true).count(), 1);
    assert_eq!(sessions[0]["user_agent"], "SoftBrowser/1.0");
    let (st, v) = b.call("POST", "/auth/sessions/others/revoke", &json!({}));
    assert_eq!((st, v["revoked"].as_u64()), (200, Some(1)));
    assert_eq!(other.call("GET", "/auth/me", &json!({})).0, 401);
    assert_eq!(b.call("GET", "/auth/me", &json!({})).0, 200);
}

#[test]
fn the_last_admin_is_protected_and_deleting_a_user_ends_them() {
    let f = Fixture::new();
    let mut boss = admin_browser(&f.svc);
    let (_, users) = boss.call("GET", "/auth/users", &json!({}));
    let boss_id = users["users"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(users["users"][0]["last_admin"], true);
    let (st, v) = boss.call(
        "POST",
        &format!("/auth/users/{boss_id}/role"),
        &json!({ "role": "viewer" }),
    );
    assert_eq!((st, v["error"].as_str()), (409, Some("last_admin")));
    let (st, _) = boss.call("DELETE", &format!("/auth/users/{boss_id}"), &json!({}));
    assert_eq!(st, 409);

    let (mut v1, _) = passkey_user(&f.svc, "V1EW", "viewer");
    let vid = f
        .svc
        .read(|d| d.user_by_name("V1EW").unwrap().id.to_string());
    let (st, v) = boss.call(
        "DELETE",
        &format!(
            "/auth/users/{vid}/credentials/{}",
            f.svc.read(|d| d
                .credentials_of(d.user_by_name("V1EW").unwrap().id)
                .next()
                .unwrap()
                .id
                .clone())
        ),
        &json!({}),
    );
    assert_eq!((st, v["error"].as_str()), (409, Some("last_credential")));
    // Promote, then the original admin may step down.
    assert_eq!(
        boss.call(
            "POST",
            &format!("/auth/users/{vid}/role"),
            &json!({ "role": "admin" })
        )
        .0,
        200
    );
    assert_eq!(
        boss.call(
            "POST",
            &format!("/auth/users/{boss_id}/role"),
            &json!({ "role": "viewer" })
        )
        .0,
        200
    );
    // V1EW, now the only admin, deletes nobody's way out: delete BOSS.
    let (st, v) = v1.call("DELETE", &format!("/auth/users/{boss_id}"), &json!({}));
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        boss.call("GET", "/auth/me", &json!({})).0,
        401,
        "BOSS's session went with them"
    );
    let (_, audit) = v1.call("GET", "/auth/audit?user=V1EW", &json!({}));
    let events: Vec<&str> = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(events[0], "user_delete", "newest first: {events:?}");
}

#[test]
fn user_management_needs_admin_and_step_up() {
    let f = Fixture::new();
    let (mut viewer, _) = passkey_user(&f.svc, "V1EW", "viewer");
    assert_eq!(viewer.call("GET", "/auth/users", &json!({})).0, 403);
    assert_eq!(viewer.call("GET", "/auth/audit", &json!({})).0, 403);
    let mut boss = admin_browser(&f.svc);
    let vid = f
        .svc
        .read(|d| d.user_by_name("V1EW").unwrap().id.to_string());
    f.advance(f.svc.settings.step_up_secs + 1);
    let (st, v) = boss.call("DELETE", &format!("/auth/users/{vid}"), &json!({}));
    assert_eq!((st, v["error"].as_str()), (401, Some("reauth_required")));
    let (st, v) = boss.call("POST", &format!("/auth/users/{vid}/signout"), &json!({}));
    assert_eq!(
        (st, v["revoked"].as_u64()),
        (200, Some(1)),
        "signing someone out needs no step-up"
    );
    assert_eq!(viewer.call("GET", "/auth/me", &json!({})).0, 401);
}

#[test]
fn the_console_manages_users_but_has_no_account() {
    let f = Fixture::new();
    let _ = passkey_user(&f.svc, "W1ABC", "admin");
    let call = |method: &str, path: &str| {
        let req = Req {
            method,
            path,
            query: "",
            body: b"{}",
            host: Some("localhost:8730"),
            origin: None,
            content_type: Some("application/json"),
            sec_fetch_site: None,
            cookie: None,
            user_agent: None,
            peer: Some("127.0.0.1:5000".parse().unwrap()),
            forwarded: false,
            surface: Surface::Control,
            tls: false,
        };
        super::handle(&f.svc, &req).unwrap().status
    };
    assert_eq!(call("GET", "/auth/users"), 200);
    assert_eq!(
        call("POST", "/auth/invites"),
        400,
        "a role is needed, but the console may ask"
    );
    assert_eq!(call("GET", "/auth/credentials"), 400);
}

#[test]
fn control_port_sign_in_needs_a_loopback_peer_not_just_a_localhost_host() {
    let f = Fixture::new();
    let req = |peer: &str, forwarded: bool| Req {
        method: "GET",
        path: "/auth/status",
        query: "",
        body: b"",
        host: Some("localhost:8730"),
        origin: None,
        content_type: None,
        sec_fetch_site: None,
        cookie: None,
        user_agent: None,
        peer: Some(peer.parse().unwrap()),
        forwarded,
        surface: Surface::Control,
        tls: false,
    };
    let refused = |peer: &str, forwarded: bool| {
        super::routes::check_signin_possible(&f.svc, &req(peer, forwarded))
            .err()
            .map(|r| r.code)
    };
    assert_eq!(refused("127.0.0.1:5000", false), None);
    assert_eq!(refused("[::1]:5000", false), None);
    assert_eq!(
        refused("192.168.1.9:5000", false),
        Some("loopback_required"),
        "a LAN peer that says Host: localhost"
    );
    assert_eq!(
        refused("127.0.0.1:5000", true),
        Some("loopback_required"),
        "a proxy on the box relaying someone else"
    );
}

/// A request from the console: a loopback peer on the control port.
fn console_call(svc: &AuthService, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let bytes = serde_json::to_vec(body).unwrap();
    let req = Req {
        method,
        path,
        query: "",
        body: &bytes,
        host: Some("localhost:8730"),
        origin: None,
        content_type: Some("application/json"),
        sec_fetch_site: None,
        cookie: None,
        user_agent: None,
        peer: Some("127.0.0.1:5000".parse().unwrap()),
        forwarded: false,
        surface: Surface::Control,
        tls: false,
    };
    let resp = super::handle(svc, &req).unwrap();
    (
        resp.status,
        serde_json::from_slice(&resp.body).unwrap_or(Value::Null),
    )
}

/// ALICE, an admin, makes an admin invite; its token.
fn alice_makes_an_admin_invite(f: &Fixture) -> (Browser<'_>, String) {
    let (mut alice, _) = passkey_user(&f.svc, "ALICE", "admin");
    let (st, v) = alice.call("POST", "/auth/invites", &json!({ "role": "admin" }));
    assert_eq!(st, 200, "{v}");
    let token = v["link"]
        .as_str()
        .unwrap()
        .split_once('#')
        .unwrap()
        .1
        .to_string();
    (alice, token)
}

fn invite_state(svc: &AuthService, token: &str) -> (u16, Option<String>) {
    let (st, v) = Browser::new(svc).call("POST", "/auth/invite/info", &json!({ "token": token }));
    (st, v["error"].as_str().map(str::to_string))
}

#[test]
fn deleting_an_admin_withdraws_the_invites_they_made() {
    let f = Fixture::new();
    let (_alice, token) = alice_makes_an_admin_invite(&f);
    assert_eq!(invite_state(&f.svc, &token).0, 200, "live while ALICE is");
    let id = f.svc.read(|d| d.user_by_name("ALICE").unwrap().id);
    let (st, v) = console_call(&f.svc, "DELETE", &format!("/auth/users/{id}"), &json!({}));
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        invite_state(&f.svc, &token),
        (410, Some("invite_revoked".into())),
        "ALICE's admin invite went with her"
    );
    // MALLORY, holding the link, cannot become an admin with it.
    let mut mallory = Browser::new(&f.svc);
    let (st, v) = mallory.call(
        "POST",
        "/auth/invite/begin",
        &json!({ "token": token, "name": "MALLORY", "kind": "passkey" }),
    );
    assert_eq!((st, v["error"].as_str()), (410, Some("invite_revoked")));
    assert!(f.svc.read(|d| d.user_by_name("MALLORY").is_none()));
}

#[test]
fn demoting_an_admin_withdraws_the_invites_they_made() {
    let f = Fixture::new();
    let (mut alice, token) = alice_makes_an_admin_invite(&f);
    // A link to re-enrol herself is hers to keep.
    let (st, mine) = alice.call("POST", "/auth/reenrol", &json!({}));
    assert_eq!(st, 200, "{mine}");
    let own = mine["link"]
        .as_str()
        .unwrap()
        .split_once('#')
        .unwrap()
        .1
        .to_string();
    let mut boss = admin_browser(&f.svc);
    let id = f.svc.read(|d| d.user_by_name("ALICE").unwrap().id);
    let (st, v) = boss.call(
        "POST",
        &format!("/auth/users/{id}/role"),
        &json!({ "role": "viewer" }),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        invite_state(&f.svc, &token),
        (410, Some("invite_revoked".into()))
    );
    assert_eq!(
        invite_state(&f.svc, &own).0,
        200,
        "her own re-enrol link stays"
    );
}

#[test]
fn signing_a_user_out_everywhere_withdraws_their_invites() {
    let f = Fixture::new();
    let (_alice, token) = alice_makes_an_admin_invite(&f);
    let mut boss = admin_browser(&f.svc);
    let id = f.svc.read(|d| d.user_by_name("ALICE").unwrap().id);
    let (st, v) = boss.call("POST", &format!("/auth/users/{id}/signout"), &json!({}));
    assert_eq!((st, v["invites_revoked"].as_u64()), (200, Some(1)), "{v}");
    assert_eq!(
        invite_state(&f.svc, &token),
        (410, Some("invite_revoked".into()))
    );
}

#[test]
fn an_invite_saved_before_creator_ids_goes_by_the_creator_name() {
    let f = Fixture::new();
    let (_alice, token) = alice_makes_an_admin_invite(&f);
    // As an older build wrote it: the creator by name only.
    f.svc
        .mutate(|d| {
            let mut v = serde_json::to_value(&d.invites).unwrap();
            for i in v.as_array_mut().unwrap() {
                i.as_object_mut().unwrap().remove("created_by_id");
            }
            d.invites = serde_json::from_value(v).unwrap();
            assert!(d.invites.iter().all(|i| i.created_by_id.is_none()));
            Ok(())
        })
        .ok()
        .unwrap();
    assert_eq!(invite_state(&f.svc, &token).0, 200);
    let id = f.svc.read(|d| d.user_by_name("ALICE").unwrap().id);
    assert_eq!(
        console_call(&f.svc, "DELETE", &format!("/auth/users/{id}"), &json!({})).0,
        200
    );
    assert_eq!(
        invite_state(&f.svc, &token),
        (410, Some("invite_revoked".into()))
    );
}

/// Sign in by name in a new browser with `pk`.
fn sign_in_with<'a>(svc: &'a AuthService, name: &str, pk: &mut Pk) -> Browser<'a> {
    let mut b = Browser::new(svc);
    let (_, v) = b.call("POST", "/auth/key/begin", &json!({ "name": name }));
    let rcr: RequestChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
    let cred = pk.do_authentication(b.origin(), rcr).unwrap();
    let (st, v) = b.call("POST", "/auth/key/finish", &json!({ "credential": cred }));
    assert_eq!(st, 200, "{v}");
    b
}

fn credential_id(f: &Fixture, name: &str, nickname: &str) -> String {
    f.svc.read(|d| {
        let id = d.user_by_name(name).unwrap().id;
        d.credentials_of(id)
            .find(|c| c.nickname == nickname)
            .unwrap()
            .id
            .clone()
    })
}

#[test]
fn removing_a_credential_signs_out_the_browsers_it_signed_in() {
    let f = Fixture::new();
    let (mut b, mut lost) = passkey_user(&f.svc, "W1ABC", "admin");
    let mut spare = WebauthnAuthenticator::new(SoftPasskey::new(true));
    assert_eq!(add_passkey(&mut b, &mut spare, "Spare").0, 200);
    let mut thief = sign_in_with(&f.svc, "W1ABC", &mut lost);
    let mut laptop = sign_in_with(&f.svc, "W1ABC", &mut spare);
    let lost_id = credential_id(&f, "W1ABC", "Passkey");
    // `b` signed in with the lost one too, but it is the one removing it.
    let (st, v) = b.call(
        "DELETE",
        &format!("/auth/credentials/{lost_id}"),
        &json!({}),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["sessions_revoked"].as_u64(), Some(1), "{v}");
    assert_eq!(thief.call("GET", "/auth/me", &json!({})).0, 401);
    assert_eq!(laptop.call("GET", "/auth/me", &json!({})).0, 200);
    assert_eq!(b.call("GET", "/auth/me", &json!({})).0, 200);
}

#[test]
fn an_admin_removing_someones_credential_signs_out_its_browsers() {
    let f = Fixture::new();
    let (mut v1, mut lost) = passkey_user(&f.svc, "V1EW", "viewer");
    let mut spare = WebauthnAuthenticator::new(SoftPasskey::new(true));
    assert_eq!(add_passkey(&mut v1, &mut spare, "Spare").0, 200);
    let mut laptop = sign_in_with(&f.svc, "V1EW", &mut spare);
    // A session saved before sessions named their credential.
    let mut old = sign_in_with(&f.svc, "V1EW", &mut spare);
    let old_session = f.svc.read(|d| d.sessions.last().unwrap().id.clone());
    f.svc
        .mutate(|d| {
            let s = d.sessions.iter_mut().find(|s| s.id == old_session).unwrap();
            s.credential = None;
            Ok(())
        })
        .ok()
        .unwrap();
    let mut boss = admin_browser(&f.svc);
    let vid = f.svc.read(|d| d.user_by_name("V1EW").unwrap().id);
    let lost_id = credential_id(&f, "V1EW", "Passkey");
    let (st, v) = boss.call(
        "DELETE",
        &format!("/auth/users/{vid}/credentials/{lost_id}"),
        &json!({}),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        v1.call("GET", "/auth/me", &json!({})).0,
        401,
        "signed in with it"
    );
    assert_eq!(
        old.call("GET", "/auth/me", &json!({})).0,
        401,
        "can't tell: signed out"
    );
    assert_eq!(laptop.call("GET", "/auth/me", &json!({})).0, 200);
    let _ = &mut lost;
}

// ---- an admin always keeps a passkey or key --------------------------------------

/// Add an authenticator app from a signed-in, stepped-up browser.
fn add_app(f: &Fixture, b: &mut Browser<'_>) {
    let (st, v) = b.call("POST", "/auth/credentials/totp/begin", &json!({}));
    assert_eq!(st, 200, "{v}");
    let secret = v["secret"].as_str().unwrap().to_string();
    let (st, v) = b.call(
        "POST",
        "/auth/credentials/totp/finish",
        &json!({ "code": code(&secret, f.svc.now()), "nickname": "App" }),
    );
    assert_eq!(st, 200, "{v}");
}

#[test]
fn nobody_without_a_key_is_made_an_admin() {
    let f = Fixture::new();
    let _ = super::tests_totp::enrol_app(&f, "APPONLY", "viewer");
    let id = f.svc.read(|d| d.user_by_name("APPONLY").unwrap().id);
    let mut boss = admin_browser(&f.svc);
    let (st, v) = boss.call(
        "POST",
        &format!("/auth/users/{id}/role"),
        &json!({ "role": "admin" }),
    );
    assert_eq!(
        (st, v["error"].as_str()),
        (409, Some("key_required")),
        "{v}"
    );
    assert!(
        v["message"].as_str().unwrap().contains("APPONLY"),
        "names who: {v}"
    );
    assert_eq!(
        f.svc.read(|d| d.user(id).unwrap().role),
        super::store::Role::Viewer
    );
    let (_, users) = boss.call("GET", "/auth/users", &json!({}));
    let row = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["name"] == "APPONLY")
        .unwrap()
        .clone();
    assert_eq!(
        (row["passkeys"].as_u64(), row["security_keys"].as_u64()),
        (Some(0), Some(0))
    );
}

#[test]
fn an_admins_last_key_stays_even_with_an_app() {
    let f = Fixture::new();
    let (mut b, _) = passkey_user(&f.svc, "W1ABC", "admin");
    add_app(&f, &mut b);
    let key = credential_id(&f, "W1ABC", "Passkey");
    let (_, list) = b.call("GET", "/auth/credentials", &json!({}));
    let row = list["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == key.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        (row["last"].as_bool(), row["last_key"].as_bool()),
        (Some(false), Some(true)),
        "{row}"
    );
    let (st, v) = b.call("DELETE", &format!("/auth/credentials/{key}"), &json!({}));
    assert_eq!((st, v["error"].as_str()), (409, Some("last_key")), "{v}");
    let id = f.svc.read(|d| d.user_by_name("W1ABC").unwrap().id);
    let mut boss = admin_browser(&f.svc);
    let (st, v) = boss.call(
        "DELETE",
        &format!("/auth/users/{id}/credentials/{key}"),
        &json!({}),
    );
    assert_eq!((st, v["error"].as_str()), (409, Some("last_key")), "{v}");
    // The app may go; and with a second key, the first may too.
    let app = credential_id(&f, "W1ABC", "App");
    let (st, v) = b.call("DELETE", &format!("/auth/credentials/{app}"), &json!({}));
    assert_eq!(st, 200, "{v}");
    add_app(&f, &mut b);
    let mut spare = WebauthnAuthenticator::new(SoftPasskey::new(true));
    assert_eq!(add_passkey(&mut b, &mut spare, "Spare").0, 200);
    let (st, v) = b.call("DELETE", &format!("/auth/credentials/{key}"), &json!({}));
    assert_eq!(st, 200, "{v}");
}

#[test]
fn a_viewers_last_key_may_go_while_an_app_remains() {
    let f = Fixture::new();
    let (mut b, _) = passkey_user(&f.svc, "V1EW", "viewer");
    add_app(&f, &mut b);
    let key = credential_id(&f, "V1EW", "Passkey");
    let (st, v) = b.call("DELETE", &format!("/auth/credentials/{key}"), &json!({}));
    assert_eq!(st, 200, "{v}");
}
