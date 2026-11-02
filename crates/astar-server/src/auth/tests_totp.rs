// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Authenticator-app sign-in through the handlers, with a fixed clock.

use serde_json::{Value, json};
use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse};

use super::service::tests::Fixture;
use super::tests_e2e::{Browser, invite};
use super::totp;

/// The code an app shows at `time` for a Base32 secret.
pub(crate) fn code(secret_b32: &str, time: u64) -> String {
    let secret = totp_rs::Secret::try_from_base32(secret_b32).unwrap();
    totp_rs::Builder::new()
        .with_algorithm(totp_rs::Algorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(30)
        .with_secret(secret.as_ref().to_vec())
        .with_account_name("x".to_string())
        .build()
        .unwrap()
        .generate(time)
        .to_string()
}

/// Enrol an app through an invite; the Base32 secret.
pub(crate) fn enrol_app(f: &Fixture, name: &str, role: &str) -> String {
    let token = invite(&f.svc, &json!({ "role": role }));
    let mut b = Browser::new(&f.svc);
    let (st, v) = b.call(
        "POST",
        "/auth/invite/totp/begin",
        &json!({ "token": token, "name": name }),
    );
    assert_eq!(st, 200, "{v}");
    let secret = v["secret"].as_str().unwrap().to_string();
    assert!(v["uri"].as_str().unwrap().starts_with("otpauth://totp/"));
    assert_ne!(
        v["qr"].as_array().unwrap().as_slice(),
        [] as [serde_json::Value; 0]
    );
    let (st, v) = b.call(
        "POST",
        "/auth/invite/totp/finish",
        &json!({ "code": code(&secret, f.svc.now()) }),
    );
    assert_eq!(st, 200, "{v}");
    secret
}

fn sign_in(f: &Fixture, b: &mut Browser<'_>, name: &str, c: &str) -> (u16, Value) {
    let _ = f;
    b.call("POST", "/auth/totp", &json!({ "name": name, "code": c }))
}

#[test]
fn enrol_by_invite_then_sign_in_once_per_code() {
    let f = Fixture::new();
    let secret = enrol_app(&f, "T0TP", "viewer");
    // The enrolment used this step's code; the same code cannot sign in.
    let mut b = Browser::new(&f.svc);
    let (st, _) = sign_in(&f, &mut b, "T0TP", &code(&secret, f.svc.now()));
    assert_eq!(st, 401, "replayed");
    f.advance(30);
    let c = code(&secret, f.svc.now());
    let (st, v) = sign_in(&f, &mut b, "T0TP", &c);
    assert_eq!(st, 200, "{v}");
    let (st, _) = sign_in(&f, &mut Browser::new(&f.svc), "T0TP", &c);
    assert_eq!(st, 401, "a code is good once");
    // One step late is still fine; two is not.
    f.advance(60);
    let late = code(&secret, f.svc.now() - 30);
    assert_eq!(sign_in(&f, &mut Browser::new(&f.svc), "T0TP", &late).0, 200);
    f.advance(120);
    let stale = code(&secret, f.svc.now() - 60);
    assert_eq!(
        sign_in(&f, &mut Browser::new(&f.svc), "T0TP", &stale).0,
        401
    );
    let stored =
        std::fs::read_to_string(super::store::store_path(&f.svc.settings.state_dir)).unwrap();
    assert!(!stored.contains(&c), "no code is ever stored");
}

#[test]
fn a_mistyped_confirmation_can_be_retried() {
    let f = Fixture::new();
    let token = invite(&f.svc, &json!({ "role": "viewer" }));
    let mut b = Browser::new(&f.svc);
    let (_, v) = b.call(
        "POST",
        "/auth/invite/totp/begin",
        &json!({ "token": token, "name": "TYPO" }),
    );
    let secret = v["secret"].as_str().unwrap().to_string();
    let (st, v) = b.call(
        "POST",
        "/auth/invite/totp/finish",
        &json!({ "code": "000000" }),
    );
    assert_eq!((st, v["error"].as_str()), (400, Some("bad_code")));
    let (st, v) = b.call(
        "POST",
        "/auth/invite/totp/finish",
        &json!({ "code": code(&secret, f.svc.now()) }),
    );
    assert_eq!(st, 200, "{v}");
}

#[test]
fn five_failures_lock_and_lockouts_double() {
    let f = Fixture::new();
    let secret = enrol_app(&f, "L0CK", "viewer");
    let fail_five = |f: &Fixture| {
        for _ in 0..totp::FAILURES_PER_LOCKOUT {
            let (st, _) = sign_in(f, &mut Browser::new(&f.svc), "L0CK", "000000");
            assert_eq!(st, 401);
        }
    };
    fail_five(&f);
    f.advance(30);
    let (st, v) = sign_in(
        &f,
        &mut Browser::new(&f.svc),
        "L0CK",
        &code(&secret, f.svc.now()),
    );
    assert_eq!(
        (st, v["error"].as_str()),
        (429, Some("too_many_codes")),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().contains("15 minutes"), "{v}");
    f.advance(15 * 60);
    fail_five(&f);
    let until = f
        .svc
        .read(|d| totp::any_locked_until(&d.user_by_name("L0CK").unwrap().totp));
    assert_eq!(
        until - f.svc.now(),
        30 * 60,
        "the second lockout is twice the first"
    );
    let lockouts = f
        .svc
        .read(|d| d.audit.iter().filter(|e| e.event == "totp_lockout").count());
    assert_eq!(lockouts, 2);
}

#[test]
fn a_passkey_sign_in_resets_the_lockout_and_reports_the_failures() {
    let f = Fixture::new();
    // A passkey first, then an app added from the account.
    let token = invite(&f.svc, &json!({ "role": "viewer" }));
    let mut b = Browser::new(&f.svc);
    let mut pk = WebauthnAuthenticator::new(SoftPasskey::new(true));
    let (st, v) = b.call(
        "POST",
        "/auth/invite/begin",
        &json!({ "token": token, "name": "B0TH", "kind": "passkey" }),
    );
    assert_eq!(st, 200, "{v}");
    let ccr: CreationChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
    let reg = pk.do_registration(b.origin(), ccr).unwrap();
    assert_eq!(
        b.call("POST", "/auth/invite/finish", &json!({ "credential": reg }))
            .0,
        200
    );
    let (st, v) = b.call("POST", "/auth/credentials/totp/begin", &json!({}));
    assert_eq!(st, 200, "{v}");
    let secret = v["secret"].as_str().unwrap().to_string();
    let (st, v) = b.call(
        "POST",
        "/auth/credentials/totp/finish",
        &json!({ "code": code(&secret, f.svc.now()), "nickname": "Phone" }),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["credential"]["nickname"], "Phone");

    for _ in 0..totp::FAILURES_PER_LOCKOUT {
        sign_in(&f, &mut Browser::new(&f.svc), "B0TH", "000000");
    }
    assert!(
        f.svc
            .read(|d| totp::any_locked_until(&d.user_by_name("B0TH").unwrap().totp))
            > f.svc.now()
    );
    // The passkey still works, says how many codes failed, and lifts the lock.
    let mut fresh = Browser::new(&f.svc);
    let (_, v) = fresh.call("POST", "/auth/key/begin", &json!({ "name": "B0TH" }));
    let rcr: RequestChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
    let cred = pk.do_authentication(fresh.origin(), rcr).unwrap();
    let (st, v) = fresh.call("POST", "/auth/key/finish", &json!({ "credential": cred }));
    assert_eq!((st, v["failed_codes"].as_u64()), (200, Some(5)), "{v}");
    let t = f.svc.read(|d| d.user_by_name("B0TH").unwrap().totp.clone());
    assert_eq!(totp::any_locked_until(&t), 0);
    let here = &t.sources[0];
    assert_eq!((here.locked_until, here.level, here.count), (0, 0, 0));
    assert_eq!(t.since_sign_in, 0);
    f.advance(30);
    assert_eq!(
        sign_in(
            &f,
            &mut Browser::new(&f.svc),
            "B0TH",
            &code(&secret, f.svc.now())
        )
        .0,
        200
    );
}

#[test]
fn an_ip_runs_out_of_failed_codes() {
    let f = Fixture::new();
    let secret = enrol_app(&f, "IPLIM", "viewer");
    for i in 0..crate::auth::limits::TOTP_FAILURES_PER_HOUR {
        // Spread across names, so no one user locks.
        let name = format!("U{i}");
        sign_in(&f, &mut Browser::new(&f.svc), &name, "000000");
    }
    f.advance(30);
    let good = code(&secret, f.svc.now());
    let (st, _) = sign_in(&f, &mut Browser::new(&f.svc), "IPLIM", &good);
    assert_eq!(st, 429, "this address is out of tries");
    let mut other = Browser::new(&f.svc);
    other.ip = "198.51.100.20:5000";
    assert_eq!(
        sign_in(&f, &mut other, "IPLIM", &good).0,
        200,
        "another address is not"
    );
}

/// A browser at its own address.
fn from(svc: &super::service::AuthService, ip: String) -> Browser<'_> {
    let mut b = Browser::new(svc);
    b.ip = Box::leak(ip.into_boxed_str());
    b
}

#[test]
fn strangers_cannot_keep_the_owner_out() {
    let f = Fixture::new();
    // Enrolled, and so signed in, from the default address.
    let secret = enrol_app(&f, "OWNER", "viewer");
    // A day of wrong codes from address after address.
    let mut evaluated = 0;
    for i in 0..(24 * 12) {
        let mut b = from(&f.svc, format!("198.51.{}.{}:4000", i / 250, i % 250 + 1));
        for _ in 0..totp::FAILURES_PER_LOCKOUT {
            let (st, _) = sign_in(&f, &mut b, "OWNER", "000000");
            assert!(st == 401 || st == 429, "{st}");
            evaluated += u64::from(st == 401);
        }
        f.advance(300);
        let until = f
            .svc
            .read(|d| d.user_by_name("OWNER").unwrap().totp.locked_until);
        assert!(
            until <= f.svc.now() + totp::STRANGER_LOCKOUT_SECS,
            "the strangers' lockout never grows past its cap"
        );
    }
    // The owner, where they have signed in before, is not locked out at all.
    let (st, v) = sign_in(
        &f,
        &mut Browser::new(&f.svc),
        "OWNER",
        &code(&secret, f.svc.now()),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["failed_codes"].as_u64(), Some(evaluated), "{v}");
    assert!(
        evaluated <= 100,
        "{evaluated} codes tried in a day from {} addresses",
        24 * 12
    );
    // Somewhere new, one more burst of strangers' failures holds them back
    // for no longer than the cap.
    let mut b = from(&f.svc, "192.0.2.77:4000".into());
    for _ in 0..totp::FAILURES_PER_LOCKOUT {
        sign_in(&f, &mut b, "OWNER", "000000");
    }
    let mut owner_away = from(&f.svc, "198.18.0.5:4000".into());
    f.advance(30);
    let (st, _) = sign_in(&f, &mut owner_away, "OWNER", &code(&secret, f.svc.now()));
    assert_eq!(st, 429, "a new address waits out the strangers' lockout");
    f.advance(totp::STRANGER_LOCKOUT_SECS);
    let (st, v) = sign_in(&f, &mut owner_away, "OWNER", &code(&secret, f.svc.now()));
    assert_eq!(st, 200, "{v}");
}

#[test]
fn the_owners_own_failures_escalate_at_their_address() {
    let f = Fixture::new();
    let secret = enrol_app(&f, "OWN2", "viewer");
    let fail_five = |f: &Fixture| {
        for _ in 0..totp::FAILURES_PER_LOCKOUT {
            sign_in(f, &mut Browser::new(&f.svc), "OWN2", "000000");
        }
    };
    fail_five(&f);
    f.advance(30);
    let (st, v) = sign_in(
        &f,
        &mut Browser::new(&f.svc),
        "OWN2",
        &code(&secret, f.svc.now()),
    );
    assert_eq!(st, 429, "{v}");
    assert!(v["message"].as_str().unwrap().contains("15 minutes"), "{v}");
    f.advance(15 * 60);
    fail_five(&f);
    let (st, v) = sign_in(
        &f,
        &mut Browser::new(&f.svc),
        "OWN2",
        &code(&secret, f.svc.now()),
    );
    assert_eq!(st, 429, "{v}");
    assert!(v["message"].as_str().unwrap().contains("30 minutes"), "{v}");
    // Somewhere else is unaffected by the owner's own lockout.
    f.advance(30);
    let mut elsewhere = from(&f.svc, "198.18.0.9:4000".into());
    let (st, v) = sign_in(&f, &mut elsewhere, "OWN2", &code(&secret, f.svc.now()));
    assert_eq!(st, 200, "{v}");
}

#[test]
fn a_strangers_address_escalates_to_a_day() {
    let f = Fixture::new();
    let _ = enrol_app(&f, "OWN3", "viewer");
    let mut b = from(&f.svc, "192.0.2.200:4000".into());
    // However long it keeps going, one address gets only a few a day.
    let mut evaluated = 0;
    for _ in 0..(2 * 24 * 60) {
        let (st, _) = sign_in(&f, &mut b, "OWN3", "000000");
        if st == 401 {
            evaluated += 1;
        }
        f.advance(60);
    }
    assert!(evaluated <= 60, "{evaluated} codes tried in two days");
}

/// `n` threads at once, each with its own browser made by `make`, each
/// calling `go`; how many answers were `401 bad_code` — codes evaluated.
fn at_once<'a>(
    n: usize,
    make: impl Fn() -> Browser<'a> + Sync,
    go: impl Fn(&mut Browser<'a>) -> (u16, Value) + Sync,
) -> usize {
    let barrier = std::sync::Barrier::new(n);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..n)
            .map(|_| {
                s.spawn(|| {
                    let mut b = make();
                    barrier.wait();
                    go(&mut b)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|(st, v)| *st == 401 && v["error"] == "bad_code")
            .count()
    })
}

#[test]
fn concurrent_wrong_codes_get_no_more_than_the_allowance() {
    let f = Fixture::new();
    let _ = enrol_app(&f, "RACE", "viewer");
    let evaluated = at_once(
        200,
        || Browser::new(&f.svc),
        |b| {
            b.call(
                "POST",
                "/auth/totp",
                &json!({ "name": "RACE", "code": "000000" }),
            )
        },
    );
    assert!(
        evaluated <= totp::FAILURES_PER_LOCKOUT as usize,
        "{evaluated} codes evaluated"
    );
    let t = f.svc.read(|d| d.user_by_name("RACE").unwrap().totp.clone());
    assert_eq!(t.since_sign_in as usize, evaluated, "each one counted");
}

#[test]
fn concurrent_wrong_step_up_codes_get_no_more_than_the_allowance() {
    let f = Fixture::new();
    let secret = enrol_app(&f, "RACE2", "viewer");
    f.advance(30);
    let mut signed = Browser::new(&f.svc);
    let (st, _) = sign_in(&f, &mut signed, "RACE2", &code(&secret, f.svc.now()));
    assert_eq!(st, 200);
    let jar = signed.jar.clone();
    let evaluated = at_once(
        200,
        || {
            let mut b = Browser::new(&f.svc);
            b.jar.clone_from(&jar);
            b
        },
        |b| b.call("POST", "/auth/stepup/totp", &json!({ "code": "000000" })),
    );
    assert!(
        evaluated <= totp::FAILURES_PER_LOCKOUT as usize,
        "{evaluated} codes evaluated"
    );
}

/// What a stranger at one address sees, trying wrong codes for `name` on a
/// schedule: every answer's status, code and sentence.
fn probe(f: &Fixture, name: &str, ip: &str) -> Vec<(u16, Value, Value)> {
    let mut b = from(&f.svc, ip.to_string());
    let mut seen = Vec::new();
    for round in 0..4 {
        for _ in 0..(totp::FAILURES_PER_LOCKOUT + 2) {
            let (st, v) = sign_in(f, &mut b, name, "000000");
            seen.push((st, v["error"].clone(), v["message"].clone()));
        }
        f.advance(if round == 1 {
            60
        } else {
            totp::STRANGER_LOCKOUT_SECS
        });
    }
    seen
}

#[test]
fn an_unknown_name_is_locked_out_like_a_real_one() {
    let f = Fixture::new();
    let _ = enrol_app(&f, "REAL", "viewer");
    let real = probe(&f, "REAL", "192.0.2.10:4000");
    let nobody = probe(&f, "N0B0DY", "192.0.2.11:4000");
    assert!(
        real.iter().any(|(st, ..)| *st == 429),
        "the real name does lock: {real:?}"
    );
    assert_eq!(real, nobody, "and nobody's name answers the same");
}

#[test]
fn rotating_through_a_64_is_one_source_and_one_audit_line() {
    let f = Fixture::new();
    let mut statuses = Vec::new();
    for i in 0..(crate::auth::limits::TOTP_FAILURES_PER_HOUR + 5) {
        let mut b = from(&f.svc, format!("[2001:db8:9:9::{:x}]:4000", i + 1));
        statuses.push(sign_in(&f, &mut b, &format!("N{i}"), "000000").0);
    }
    assert_eq!(
        statuses.iter().filter(|&&s| s == 401).count(),
        crate::auth::limits::TOTP_FAILURES_PER_HOUR,
        "one /64's allowance, not one per address: {statuses:?}"
    );
    let lines: Vec<(String, u32)> = f.svc.read(|d| {
        d.audit
            .iter()
            .filter(|e| e.event == "sign_in_failed")
            .map(|e| (e.ip.clone(), e.times))
            .collect()
    });
    assert_eq!(
        lines,
        [(
            "2001:db8:9:9::/64".to_string(),
            u32::try_from(crate::auth::limits::TOTP_FAILURES_PER_HOUR).unwrap()
        )]
    );
}

// ---- admins sign in with a passkey or key, not codes ------------------------------

type Pk = WebauthnAuthenticator<SoftPasskey>;

/// A user enrolled with a passkey through an invite, signed in, with an
/// authenticator app added from their account; the browser, the passkey
/// and the app's Base32 secret.
fn key_and_app_user<'a>(f: &'a Fixture, name: &str, role: &str) -> (Browser<'a>, Pk, String) {
    let token = invite(&f.svc, &json!({ "role": role }));
    let mut b = Browser::new(&f.svc);
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
    let secret = add_app(f, &mut b);
    (b, pk, secret)
}

/// Add an authenticator app from a signed-in, stepped-up browser.
fn add_app(f: &Fixture, b: &mut Browser<'_>) -> String {
    let (st, v) = b.call("POST", "/auth/credentials/totp/begin", &json!({}));
    assert_eq!(st, 200, "{v}");
    let secret = v["secret"].as_str().unwrap().to_string();
    let (st, v) = b.call(
        "POST",
        "/auth/credentials/totp/finish",
        &json!({ "code": code(&secret, f.svc.now()) }),
    );
    assert_eq!(st, 200, "{v}");
    secret
}

#[test]
fn an_admins_right_code_fails_exactly_like_a_wrong_one() {
    let f = Fixture::new();
    let (mut home, _, secret) = key_and_app_user(&f, "ADM1", "admin");
    let _ = enrol_app(&f, "VIEW1", "viewer");
    f.advance(30);
    let away = "198.51.100.7:4000";
    let right = sign_in(
        &f,
        &mut from(&f.svc, away.into()),
        "ADM1",
        &code(&secret, f.svc.now()),
    );
    let wrong = sign_in(&f, &mut from(&f.svc, away.into()), "ADM1", "000000");
    let viewer_wrong = sign_in(
        &f,
        &mut from(&f.svc, "198.51.100.8:4000".into()),
        "VIEW1",
        "000000",
    );
    assert_eq!(right.0, 401, "{right:?}");
    assert_eq!(right, wrong, "a right code says what a wrong one does");
    assert_eq!(right, viewer_wrong, "and what a viewer's wrong code does");
    // Counted like one: the fifth from that address sets off the lockout,
    // and the audit rolls them up into one line.
    for _ in 0..3 {
        f.advance(30);
        let (st, _) = sign_in(
            &f,
            &mut from(&f.svc, away.into()),
            "ADM1",
            &code(&secret, f.svc.now()),
        );
        assert_eq!(st, 401);
    }
    f.advance(30);
    let (st, v) = sign_in(
        &f,
        &mut from(&f.svc, away.into()),
        "ADM1",
        &code(&secret, f.svc.now()),
    );
    assert_eq!(
        (st, v["error"].as_str()),
        (429, Some("too_many_codes")),
        "{v}"
    );
    let (failed, times) = f.svc.read(|d| {
        let u = d.user_by_name("ADM1").unwrap();
        let line = d
            .audit
            .iter()
            .find(|e| e.event == "sign_in_failed" && e.user.as_deref() == Some("ADM1"))
            .map(|e| (e.detail.clone(), e.times));
        (u.totp.since_sign_in, line)
    });
    assert_eq!(failed, 5);
    assert_eq!(times, Some(("authenticator app".to_string(), 5)));
    // No step was spent: the admin's own session can still step up with
    // the current code (it needs a session, so it is not a guess surface).
    let (st, v) = home.call(
        "POST",
        "/auth/stepup/totp",
        &json!({ "code": code(&secret, f.svc.now()) }),
    );
    assert_eq!(st, 200, "{v}");
}

#[test]
fn a_viewer_with_a_key_still_signs_in_with_codes() {
    let f = Fixture::new();
    let (_, _, secret) = key_and_app_user(&f, "VIEW2", "viewer");
    f.advance(30);
    let (st, v) = sign_in(
        &f,
        &mut Browser::new(&f.svc),
        "VIEW2",
        &code(&secret, f.svc.now()),
    );
    assert_eq!(st, 200, "{v}");
}

#[test]
fn an_admin_invite_cannot_be_redeemed_with_an_app() {
    let f = Fixture::new();
    let token = invite(&f.svc, &json!({ "role": "admin" }));
    let mut b = Browser::new(&f.svc);
    let (st, info) = b.call("POST", "/auth/invite/info", &json!({ "token": token }));
    assert_eq!((st, &info["app_allowed"]), (200, &json!(false)), "{info}");
    let (st, v) = b.call(
        "POST",
        "/auth/invite/totp/begin",
        &json!({ "token": token, "name": "NEWADM" }),
    );
    assert_eq!(
        (st, v["error"].as_str()),
        (403, Some("key_required")),
        "{v}"
    );
    assert!(v.get("secret").is_none(), "no secret is shown: {v}");
    assert!(f.svc.read(|d| d.user_by_name("NEWADM").is_none()));
    // The invite is still good for a passkey.
    let (st, _) = b.call("POST", "/auth/invite/info", &json!({ "token": token }));
    assert_eq!(st, 200);
    let token = invite(&f.svc, &json!({ "role": "viewer" }));
    let (_, info) = b.call("POST", "/auth/invite/info", &json!({ "token": token }));
    assert_eq!(info["app_allowed"], true, "{info}");
}

#[test]
fn a_re_enrol_invite_that_ends_at_an_admin_takes_no_app() {
    let f = Fixture::new();
    let (_, _, _) = key_and_app_user(&f, "PROMO", "viewer");
    let id = f
        .svc
        .read(|d| d.user_by_name("PROMO").unwrap().id.to_string());
    let token = invite(&f.svc, &json!({ "user_id": id }));
    let mut phone = Browser::new(&f.svc);
    let (st, v) = phone.call(
        "POST",
        "/auth/invite/totp/begin",
        &json!({ "token": token }),
    );
    assert_eq!(st, 200, "a viewer may: {v}");
    let secret = v["secret"].as_str().unwrap().to_string();
    // Promoted while the app is being set up: the finish is refused, in
    // the same store write that would have saved it.
    let mut boss = super::tests_e2e::admin_browser(&f.svc);
    let (st, v) = boss.call(
        "POST",
        &format!("/auth/users/{id}/role"),
        &json!({ "role": "admin" }),
    );
    assert_eq!(st, 200, "{v}");
    let (st, v) = phone.call(
        "POST",
        "/auth/invite/totp/finish",
        &json!({ "code": code(&secret, f.svc.now()) }),
    );
    assert_eq!(
        (st, v["error"].as_str()),
        (403, Some("key_required")),
        "{v}"
    );
    let apps = f.svc.read(|d| {
        let u = d.user_by_name("PROMO").unwrap().id;
        d.credentials_of(u)
            .filter(|c| c.kind.name() == "totp")
            .count()
    });
    assert_eq!(apps, 1, "only the app added before");
}

#[test]
fn an_admin_with_only_codes_from_before_keeps_them_until_a_key_is_added() {
    let f = Fixture::new();
    // Written before the rule: an admin whose only way in is an app.
    let secret = enrol_app(&f, "OLDADM", "viewer");
    f.svc
        .mutate(|d| {
            let id = d.user_by_name("OLDADM").unwrap().id;
            d.user_mut(id).unwrap().role = super::store::Role::Admin;
            Ok(())
        })
        .ok()
        .unwrap();
    assert_eq!(
        f.svc.read(super::store::AuthData::admins_without_a_key),
        ["OLDADM".to_string()],
        "what the warning at load names"
    );
    // Reopened from disk, the admin still gets in with a code.
    let svc = super::service::AuthService::with_clock(f.svc.settings.clone(), {
        let c = std::sync::Arc::clone(&f.clock);
        std::sync::Arc::new(move || c.load(std::sync::atomic::Ordering::SeqCst))
    });
    f.advance(30);
    let mut b = Browser::new(&svc);
    let (st, v) = b.call(
        "POST",
        "/auth/totp",
        &json!({ "name": "OLDADM", "code": code(&secret, svc.now()) }),
    );
    assert_eq!((st, v["role"].as_str()), (200, Some("admin")), "{v}");
    let (_, me) = b.call("GET", "/auth/me", &json!({}));
    assert_eq!(me["totp_only"], true, "the account page nudges: {me}");
    // Once a passkey is added, codes stop signing them in.
    let mut pk = WebauthnAuthenticator::new(SoftPasskey::new(true));
    let (st, v) = b.call("POST", "/auth/credentials/webauthn/begin", &json!({}));
    assert_eq!(st, 200, "{v}");
    let ccr: CreationChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
    let reg = pk.do_registration(b.origin(), ccr).unwrap();
    let (st, v) = b.call(
        "POST",
        "/auth/credentials/webauthn/finish",
        &json!({ "credential": reg }),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        svc.read(super::store::AuthData::admins_without_a_key),
        [] as [std::string::String; 0]
    );
    f.advance(30);
    let (st, v) = Browser::new(&svc).call(
        "POST",
        "/auth/totp",
        &json!({ "name": "OLDADM", "code": code(&secret, svc.now()) }),
    );
    assert_eq!((st, v["error"].as_str()), (401, Some("bad_code")), "{v}");
    // The passkey does.
    let mut fresh = Browser::new(&svc);
    let (_, v) = fresh.call("POST", "/auth/key/begin", &json!({ "name": "OLDADM" }));
    let rcr: RequestChallengeResponse = serde_json::from_value(v["options"].clone()).unwrap();
    let cred = pk.do_authentication(fresh.origin(), rcr).unwrap();
    assert_eq!(
        fresh
            .call("POST", "/auth/key/finish", &json!({ "credential": cred }))
            .0,
        200
    );
}
