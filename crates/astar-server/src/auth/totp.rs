// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Authenticator apps (TOTP): RFC 6238, SHA-1, six digits, 30-second
//! steps, one step either side — what every app honours.
//!
//! A code is accepted once: the last accepted step is stored per
//! credential and anything at or before it is refused.
//!
//! Failed codes are held back in two ways, so that nobody else can lock
//! the owner out of their own codes:
//!
//! * **Per source** (an IPv4 address or an IPv6 /64) against each user:
//!   five failures lock that source out for 15 minutes, doubling with each
//!   further lockout up to a day; only a sign-in from that source resets
//!   it. The owner's own mistakes escalate here, at their own address.
//! * **Strangers**, every source that has never signed in as that user,
//!   together: five failures lock all of them out for
//!   [`STRANGER_LOCKOUT_SECS`], and after that each further failure does
//!   again, until a day passes without one. That lockout never grows, and
//!   a source the user has signed in from ignores it.
//!
//! So the guess rate against one user is bounded at about five a day per
//! source once escalated (the design's bound, for an attacker with one
//! address), and at 4 + 86 400 / [`STRANGER_LOCKOUT_SECS`] = 100 a day
//! across every address an attacker can muster. A sustained year of that
//! is about 36 500 guesses, each with a 3 in 10⁶ chance against a six-digit
//! code with a step either side: roughly a 10% chance, with every lockout in
//! the audit and the owner told on their next sign-in. Each source also has
//! 30 failed codes an hour across all users. Passkeys and keys are never
//! locked out.
//!
//! That bound is accepted for viewers, not for admins: **codes never sign
//! an admin in.** An admin's code at `/auth/totp` fails exactly as a wrong
//! one does, right or not, so the answer names nobody as an admin; an admin
//! signs in with a passkey or key and may use a code only to step up in a
//! session they already have. So an admin always keeps a key: an invite
//! that ends at an admin cannot be redeemed with an app, nobody without a
//! key is promoted, and an admin's last key cannot be removed. The one
//! exception, an admin from before this rule with no key at all, is in
//! [`AuthData::codes_sign_in`].
//!
//! The secret is shown once, at enrolment, and saved only after the user's
//! app proves it has it. Nothing here logs a secret or a code.

use serde::Deserialize;
use serde_json::json;
use totp_rs::{Algorithm, Secret, Totp};
use uuid::Uuid;

use crate::http::HttpResponse;

use super::ceremony::{Ceremony, Enrol, Kind};
use super::flows::{
    begin_checks, body, clean_text, enrolment_for_invite, invite_role, key_required, live_invite,
    need_step_up, need_user, ok_after_add, redeem_within, sign_in_within, signed_in_response,
    take_ceremony, with_ceremony,
};
use super::routes::check_signin_possible;
use super::service::{AuthService, Refusal, Req};
use super::session;
use super::store::{
    AuditEntry, AuthData, Credential, CredentialKind, Role, SourceFailures, TotpFailures,
};

/// Digits in a code.
pub const DIGITS: u8 = 6;
/// Seconds per step.
pub const STEP: u64 = 30;
/// Failures before a lockout.
pub const FAILURES_PER_LOCKOUT: u32 = 5;
/// The first lockout.
pub const FIRST_LOCKOUT_SECS: u64 = 15 * 60;
/// The longest lockout.
pub const MAX_LOCKOUT_SECS: u64 = 24 * 3600;
/// The strangers' lockout: how long codes from every address that has
/// never signed in as the user are refused. It never grows.
pub const STRANGER_LOCKOUT_SECS: u64 = 15 * 60;
/// Wrong confirmation codes allowed while enrolling one secret.
pub const ENROL_ATTEMPTS: u8 = 5;

/// Sources remembered per user; past this the least telling is forgotten.
pub const MAX_SOURCES: usize = 16;
/// A day without a stranger's failure ends the strangers' run.
pub const STRANGER_QUIET_SECS: u64 = 24 * 3600;

/// When codes from `key` for this user are refused until, or `None` while
/// they are not.
#[must_use]
pub fn locked_until(t: &TotpFailures, key: &str, now: u64) -> Option<u64> {
    let src = t.sources.iter().find(|s| s.key == key);
    let mut until = src.map_or(0, |s| s.locked_until);
    if !src.is_some_and(|s| s.trusted) {
        until = until.max(strangers_until(t));
    }
    (until > now).then_some(until)
}

/// The strangers' lockout, never longer than [`STRANGER_LOCKOUT_SECS`]
/// after their last failure, whatever an older store says.
fn strangers_until(t: &TotpFailures) -> u64 {
    t.locked_until
        .min(t.last_failure.saturating_add(STRANGER_LOCKOUT_SECS))
}

/// The latest any source is locked out until: what the users list shows.
#[must_use]
pub fn any_locked_until(t: &TotpFailures) -> u64 {
    t.sources
        .iter()
        .map(|s| s.locked_until)
        .fold(strangers_until(t), u64::max)
}

/// `key`'s entry, made if need be. When the list is full, an untrusted,
/// unlocked, long-quiet source goes first and a trusted one last.
fn source_mut<'a>(t: &'a mut TotpFailures, key: &str, now: u64) -> &'a mut SourceFailures {
    if let Some(i) = t.sources.iter().position(|s| s.key == key) {
        return &mut t.sources[i];
    }
    if t.sources.len() >= MAX_SOURCES
        && let Some(i) = t
            .sources
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| (s.trusted, s.locked_until > now, s.last))
            .map(|(i, _)| i)
    {
        t.sources.swap_remove(i);
    }
    t.sources.push(SourceFailures {
        key: key.to_string(),
        last: now,
        ..SourceFailures::default()
    });
    t.sources.last_mut().expect("just pushed")
}

/// Names tracked for [`Decoys`] at once.
pub const MAX_DECOYS: usize = 4096;

/// Lockout bookkeeping for names nobody has, in memory: a wrong code for an
/// unknown name is counted and locked out exactly as one for a real name
/// is, so the 429 that follows the fifth says nothing about who exists.
/// Bounded; past [`MAX_DECOYS`] the longest-quiet name is forgotten.
#[derive(Default)]
pub struct Decoys {
    map: std::collections::HashMap<String, (TotpFailures, u64)>,
}

impl Decoys {
    /// Check and count one code for the unknown `name` from `key`: `Err`
    /// with the lockout's end, as a real user's would be, or `Ok` for a
    /// failure counted.
    ///
    /// # Errors
    /// The name is locked out for this source, until the time returned.
    pub fn attempt(&mut self, name: &str, key: &str, now: u64) -> Result<(), u64> {
        let name = name.trim().to_ascii_lowercase();
        if !self.map.contains_key(&name)
            && self.map.len() >= MAX_DECOYS
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, last))| *last)
                .map(|(k, _)| k.clone())
        {
            self.map.remove(&oldest);
        }
        let (t, last) = self.map.entry(name).or_default();
        *last = now;
        if let Some(until) = locked_until(t, key, now) {
            return Err(until);
        }
        record_failure(t, key, now);
        Ok(())
    }

    /// How many names are tracked (tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether none are.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// A lockout that a failure set off.
pub enum Lockout {
    /// This source, for this many seconds.
    Source(u64),
    /// Every stranger, for this many seconds.
    Strangers(u64),
}

/// Count a failed code from `key`; the lockouts it set off.
pub fn record_failure(t: &mut TotpFailures, key: &str, now: u64) -> Vec<Lockout> {
    let mut out = Vec::new();
    t.since_sign_in += 1;
    let src = source_mut(t, key, now);
    src.count += 1;
    src.last = now;
    let trusted = src.trusted;
    if src.count >= FAILURES_PER_LOCKOUT {
        src.count = 0;
        src.level += 1;
        let secs = lockout_secs(src.level);
        src.locked_until = now + secs;
        out.push(Lockout::Source(secs));
    }
    if !trusted {
        if now.saturating_sub(t.last_failure) >= STRANGER_QUIET_SECS {
            t.count = 0;
            t.level = 0;
        }
        t.count += 1;
        t.last_failure = now;
        let threshold = if t.level == 0 {
            FAILURES_PER_LOCKOUT
        } else {
            1
        };
        if t.count >= threshold {
            t.count = 0;
            t.level += 1;
            t.locked_until = now + STRANGER_LOCKOUT_SECS;
            out.push(Lockout::Strangers(STRANGER_LOCKOUT_SECS));
        }
    }
    out
}

/// A sign-in (with any credential) from `key`: that source's failures are
/// forgiven, and strangers no longer hold it back.
pub fn record_success(t: &mut TotpFailures, key: &str, now: u64) {
    t.since_sign_in = 0;
    let src = source_mut(t, key, now);
    *src = SourceFailures {
        key: src.key.clone(),
        last: now,
        trusted: true,
        ..SourceFailures::default()
    };
}

/// A new 160-bit secret.
#[must_use]
pub fn new_secret() -> Vec<u8> {
    session::random_bytes::<20>().to_vec()
}

fn totp(secret: Vec<u8>, issuer: Option<&str>, account: &str) -> Option<Totp> {
    totp_rs::Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(DIGITS)
        .with_skew(1)
        .with_step_duration(STEP)
        .with_secret(secret)
        .with_issuer(issuer.map(str::to_string))
        .with_account_name(account.to_string())
        .build()
        .ok()
}

/// Base32, as an app's manual entry wants it.
#[must_use]
pub fn base32(secret: &[u8]) -> String {
    Secret::from(secret.to_vec()).to_base32()
}

/// The step a code matches, newer than `last_step`, or `None`.
#[must_use]
pub fn verify(secret: &[u8], code: &str, now: u64, last_step: u64) -> Option<u64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != usize::from(DIGITS) || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let t = totp(secret.to_vec(), None, "x")?;
    let step = t.check(&code, now)?;
    (step > last_step).then_some(step)
}

/// The step a code matches for a stored Base32 secret.
#[must_use]
pub fn verify_base32(secret_b32: &str, code: &str, now: u64, last_step: u64) -> Option<u64> {
    let secret = Secret::try_from_base32(secret_b32).ok()?;
    verify(secret.as_ref(), code, now, last_step)
}

/// The `otpauth://` URI an app scans.
#[must_use]
pub fn otpauth_uri(secret: &[u8], host: &str, account: &str) -> Option<String> {
    let issuer = format!("astar {host}");
    totp(secret.to_vec(), Some(&issuer), account)?.to_url().ok()
}

/// A QR code's modules, one string of `0`/`1` per row, for the page to
/// draw with DOM nodes: no image, nothing cacheable at a URL.
#[must_use]
pub fn qr_rows(text: &str) -> Vec<String> {
    let Ok(qr) = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium) else {
        return Vec::new();
    };
    let n = qr.size();
    (0..n)
        .map(|y| {
            (0..n)
                .map(|x| if qr.get_module(x, y) { '1' } else { '0' })
                .collect()
        })
        .collect()
}

/// The lockout after the `level`th: 15 minutes, doubling, at most a day.
#[must_use]
pub fn lockout_secs(level: u32) -> u64 {
    let doublings = level.saturating_sub(1).min(16);
    (FIRST_LOCKOUT_SECS << doublings).min(MAX_LOCKOUT_SECS)
}

/// The TOTP routes, or `None` for a path that is not one.
pub fn route(svc: &AuthService, req: &Req<'_>) -> Option<Result<HttpResponse, Refusal>> {
    Some(match (req.method, req.path) {
        ("POST", "/auth/totp") => sign_in(svc, req),
        ("POST", "/auth/invite/totp/begin") => invite_begin(svc, req),
        ("POST", "/auth/invite/totp/finish") => enrol_finish(svc, req, true),
        ("POST", "/auth/credentials/totp/begin") => add_begin(svc, req),
        ("POST", "/auth/credentials/totp/finish") => enrol_finish(svc, req, false),
        ("POST", "/auth/stepup/totp") => step_up(svc, req),
        _ => return None,
    })
}

#[derive(Deserialize)]
struct CodeSignIn {
    name: String,
    code: String,
}

fn bad_code() -> Refusal {
    Refusal::new(
        401,
        "bad_code",
        "That code didn't work. Check the app and try again.",
    )
}

fn too_many_from_here() -> Refusal {
    Refusal::new(
        429,
        "too_many_codes",
        "Too many code attempts from here; try again in an hour, or use a passkey.",
    )
}

/// Inside a store write: count one failed code from `key` against `user`,
/// with its audit entries.
fn failure_within(d: &mut AuthData, user: Uuid, key: &str, ip: &str, now: u64) {
    let Some(u) = d.user_mut(user) else {
        return;
    };
    let lockouts = record_failure(&mut u.totp, key, now);
    let name = u.name.clone();
    d.audit_failure(AuditEntry {
        at: now,
        event: "sign_in_failed".into(),
        user: Some(name.clone()),
        detail: "authenticator app".into(),
        ip: key.to_string(),
        times: 0,
        since: None,
    });
    for l in lockouts {
        let detail = match l {
            Lockout::Source(secs) => {
                format!("codes from {key} locked for {} minutes", secs / 60)
            }
            Lockout::Strangers(secs) => format!(
                "codes from addresses never signed in from locked for {} minutes",
                secs / 60
            ),
        };
        d.audit(AuditEntry {
            at: now,
            event: "totp_lockout".into(),
            user: Some(name.clone()),
            detail,
            ip: ip.to_string(),
            times: 0,
            since: None,
        });
    }
}

/// Count one failed code for a name nobody has.
fn count_unknown_failure(svc: &AuthService, req: &Req<'_>, now: u64) {
    let ip = req.client_key();
    let _ = svc.mutate(|d| {
        d.audit_failure(AuditEntry {
            at: now,
            event: "sign_in_failed".into(),
            user: None,
            detail: "authenticator app".into(),
            ip,
            times: 0,
            since: None,
        });
        Ok(())
    });
}

/// Take one of this source's failed codes for the hour before a code is
/// evaluated, so concurrent requests cannot all slip under the limit; the
/// caller gives it back when the code was not a failure.
fn reserve_attempt(svc: &AuthService, key: &str, now: u64) -> Result<(), Refusal> {
    if svc
        .limits
        .lock()
        .expect("limits poisoned")
        .totp_reserve(key, now)
    {
        Ok(())
    } else {
        Err(too_many_from_here())
    }
}

fn refund_attempt(svc: &AuthService, key: &str) {
    svc.limits.lock().expect("limits poisoned").totp_refund(key);
}

/// What a code is being tried for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// Signing in, from nothing: the open guess surface.
    SignIn,
    /// Confirming it's you in a session you already have.
    StepUp,
}

/// Try `code` for `user` in one store write: refuse while this source is
/// locked out, evaluate the code, and either record the failure or accept
/// the step and run `on_success` with the credential's id — so no two requests ever see the same
/// count, and at most the allowance of codes is ever evaluated.
///
/// A sign-in for someone codes may not sign in (an admin with a key:
/// [`AuthData::codes_sign_in`]) is a failure whatever the code: evaluated
/// all the same, counted, locked out and audited exactly as a wrong code,
/// and answered with the same refusal, so it says nothing about who is an
/// admin. The role and keys are read in this same write, so a promotion or
/// a new key cannot slip between the check and the sign-in.
fn attempt<R>(
    svc: &AuthService,
    req: &Req<'_>,
    user: Uuid,
    code: &str,
    purpose: Purpose,
    on_success: impl FnOnce(&mut AuthData, u64, String) -> Result<R, Refusal>,
) -> Result<R, Refusal> {
    let now = svc.now();
    let key = req.client_key();
    let ip = req.ip();
    reserve_attempt(svc, &key, now)?;
    let out = svc.mutate(|d| {
        let u = d.user(user).ok_or_else(bad_code)?;
        if let Some(until) = locked_until(&u.totp, &key, now) {
            return Err(locked_out(until, now));
        }
        let allowed = purpose == Purpose::StepUp || d.codes_sign_in(user);
        let found = d.credentials_of(user).find_map(|c| match &c.kind {
            CredentialKind::Totp { secret, last_step } => {
                verify_base32(secret, code, now, *last_step).map(|step| (c.id.clone(), step))
            }
            _ => None,
        });
        let Some((cred_id, step)) = found.filter(|_| allowed) else {
            failure_within(d, user, &key, &ip, now);
            return Ok(None);
        };
        accept_step(d, &cred_id, step, now)?;
        on_success(d, now, cred_id).map(Some)
    });
    match out {
        Ok(Some(r)) => {
            refund_attempt(svc, &key);
            Ok(r)
        }
        Ok(None) => Err(bad_code()),
        Err(e) => {
            refund_attempt(svc, &key);
            Err(e)
        }
    }
}

fn locked_out(until: u64, now: u64) -> Refusal {
    let minutes = (until - now).div_ceil(60);
    Refusal::new(
        429,
        "too_many_codes",
        format!(
            "Too many code attempts; try again in {minutes} minute{}, or use a passkey.",
            if minutes == 1 { "" } else { "s" }
        ),
    )
}

fn sign_in(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let b: CodeSignIn = body(req)?;
    check_signin_possible(svc, req)?;
    let now = svc.now();
    let user = svc.read(|d| d.user_by_name(&b.name).map(|u| u.id));
    let Some(user) = user else {
        // Nobody by that name: the same limits, the same lockouts and the
        // same store write as a real user's wrong code.
        let key = req.client_key();
        reserve_attempt(svc, &key, now)?;
        let decoy = svc
            .decoys
            .lock()
            .expect("decoys poisoned")
            .attempt(&b.name, &key, now);
        if let Err(until) = decoy {
            refund_attempt(svc, &key);
            return Err(locked_out(until, now));
        }
        count_unknown_failure(svc, req, now);
        return Err(bad_code());
    };
    let s = attempt(svc, req, user, &b.code, Purpose::SignIn, |d, _, cred| {
        sign_in_within(svc, d, req, user, "authenticator app", Some(cred))
    })?;
    Ok(signed_in_response(svc, req, &s))
}

/// Inside a store write: record the accepted step, refusing one that a
/// concurrent request already used.
fn accept_step(d: &mut AuthData, cred_id: &str, step: u64, now: u64) -> Result<(), Refusal> {
    let c = d
        .credentials
        .iter_mut()
        .find(|c| c.id == cred_id)
        .ok_or_else(bad_code)?;
    let CredentialKind::Totp { last_step, .. } = &mut c.kind else {
        return Err(bad_code());
    };
    if step <= *last_step {
        return Err(bad_code());
    }
    *last_step = step;
    c.last_used = Some(now);
    Ok(())
}

/// The enrolment answer: the secret, its URI and the QR modules.
fn enrol_answer(
    svc: &AuthService,
    req: &Req<'_>,
    enrol: Enrol,
    user_id: Uuid,
    account: &str,
) -> Result<HttpResponse, Refusal> {
    let secret = new_secret();
    let host = req.hostname();
    let uri = otpauth_uri(&secret, &host, account).ok_or_else(super::flows::internal)?;
    let answer = json!({
        // Shown once, here; saved only after a code proves the app has it.
        "secret": base32(&secret),
        "uri": uri,
        "qr": qr_rows(&uri),
        "issuer": format!("astar {host}"),
        "account": account,
    });
    Ok(with_ceremony(
        svc,
        req,
        Kind::TotpEnrol {
            enrol,
            user_id,
            secret,
            attempts: 0,
        },
        &answer,
    ))
}

#[derive(Deserialize)]
struct InviteTotpBegin {
    token: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

fn invite_begin(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let b: InviteTotpBegin = body(req)?;
    let hash = if session::is_token(&b.token) {
        session::hash_token(&b.token)
    } else {
        return Err(Refusal::new(
            404,
            "invite_unknown",
            "This invite link isn't valid.",
        ));
    };
    begin_checks(svc, req)?;
    let (enrol, user_id, _) = enrolment_for_invite(svc, &hash, b.name, b.display_name)?;
    // Before any secret is made: an invite that ends at an admin takes a
    // passkey or key. Redemption checks again, in its own write.
    let now = svc.now();
    if svc.read(|d| live_invite(d, &hash, now).is_ok_and(|i| invite_role(d, i) == Role::Admin)) {
        return Err(key_required());
    }
    let Enrol::Invite { name, .. } = &enrol else {
        return Err(super::flows::internal());
    };
    let account = name.clone();
    enrol_answer(svc, req, enrol, user_id, &account)
}

fn add_begin(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    need_step_up(svc, &super::service::Principal::User(u.clone()))?;
    begin_checks(svc, req)?;
    enrol_answer(
        svc,
        req,
        Enrol::Existing {
            session_id: u.session_id.clone(),
        },
        u.user_id,
        &u.name,
    )
}

#[derive(Deserialize)]
struct EnrolFinish {
    code: String,
    #[serde(default)]
    nickname: Option<String>,
}

/// Finish an enrolment; `invite` says which route it came in on, and the
/// ceremony must be of that kind.
fn enrol_finish(svc: &AuthService, req: &Req<'_>, invite: bool) -> Result<HttpResponse, Refusal> {
    let b: EnrolFinish = body(req)?;
    let c = take_ceremony(svc, req)?;
    let Ceremony {
        created,
        host,
        client,
        kind:
            Kind::TotpEnrol {
                enrol,
                user_id,
                secret,
                attempts,
            },
    } = c
    else {
        return Err(Refusal::new(
            400,
            "ceremony_expired",
            "That took too long, or was started in another window. Try again.",
        ));
    };
    if invite != matches!(enrol, Enrol::Invite { .. }) {
        return Err(Refusal::new(
            400,
            "ceremony_expired",
            "That took too long, or was started in another window. Try again.",
        ));
    }
    let now = svc.now();
    let Some(step) = verify(&secret, &b.code, now, 0) else {
        // The app may simply be a step behind: let the same secret be
        // tried again, a few times, under the same cookie.
        if attempts + 1 < ENROL_ATTEMPTS
            && let Some(id) = session::cookie_value(req.cookie, session::ceremony_cookie(req.tls))
        {
            svc.ceremonies
                .lock()
                .expect("ceremonies poisoned")
                .put_back(
                    id,
                    Ceremony {
                        created,
                        host,
                        client,
                        kind: Kind::TotpEnrol {
                            enrol,
                            user_id,
                            secret,
                            attempts: attempts + 1,
                        },
                    },
                );
        }
        return Err(Refusal::new(
            400,
            "bad_code",
            "That code didn't match. Check the app shows this node, and try the newest code.",
        ));
    };
    let nickname = b
        .nickname
        .as_deref()
        .map(clean_text)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Authenticator app".to_string());
    let cred = Credential {
        id: session::new_id(),
        user_id,
        nickname,
        created: now,
        last_used: Some(now),
        kind: CredentialKind::Totp {
            secret: base32(&secret),
            last_step: step,
        },
    };
    match &enrol {
        Enrol::Invite { .. } => {
            let s = svc.mutate(|d| redeem_within(svc, d, req, &enrol, user_id, cred))?;
            Ok(signed_in_response(svc, req, &s))
        }
        Enrol::Existing { session_id } => ok_after_add(svc, req, session_id, cred),
    }
}

#[derive(Deserialize)]
struct StepUpCode {
    code: String,
}

/// Confirm it's you with a code.
fn step_up(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    let b: StepUpCode = body(req)?;
    attempt(
        svc,
        req,
        u.user_id,
        &b.code,
        Purpose::StepUp,
        |d, now, _| {
            super::manage::mark_stepped_up(d, &u.session_id, now);
            if let Some(user) = d.user_mut(u.user_id) {
                record_success(&mut user.totp, &req.client_key(), now);
            }
            d.audit(AuditEntry {
                at: now,
                event: "step_up".into(),
                user: Some(u.name.clone()),
                detail: "authenticator app".into(),
                ip: req.ip(),
                times: 0,
                since: None,
            });
            Ok(())
        },
    )?;
    Ok(super::ok_json(&json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B, SHA-1, eight digits truncated to six.
    #[test]
    fn rfc_6238_vectors() {
        let secret = b"12345678901234567890";
        let t = totp(secret.to_vec(), None, "x").unwrap();
        assert_eq!(t.generate(59).to_string(), "287082");
        assert_eq!(t.generate(1_111_111_109).to_string(), "081804");
        assert_eq!(t.generate(1_234_567_890).to_string(), "005924");
    }

    #[test]
    fn one_step_either_side_and_no_more() {
        let secret = new_secret();
        let t = totp(secret.clone(), None, "x").unwrap();
        let now = 1_800_000_000;
        let step = now / STEP;
        let at = |s: u64| t.generate(s * STEP).to_string();
        assert_eq!(verify(&secret, &at(step), now, 0), Some(step));
        assert_eq!(verify(&secret, &at(step - 1), now, 0), Some(step - 1));
        assert_eq!(verify(&secret, &at(step + 1), now, 0), Some(step + 1));
        assert_eq!(verify(&secret, &at(step - 2), now, 0), None);
        assert_eq!(verify(&secret, &at(step + 2), now, 0), None);
        assert_eq!(verify(&secret, "12345", now, 0), None);
        assert_eq!(verify(&secret, "abcdef", now, 0), None);
    }

    #[test]
    fn a_code_is_good_once() {
        let secret = new_secret();
        let t = totp(secret.clone(), None, "x").unwrap();
        let now = 1_800_000_000;
        let code = t.generate(now).to_string();
        let step = verify(&secret, &code, now, 0).unwrap();
        assert_eq!(verify(&secret, &code, now, step), None, "replayed");
        assert_eq!(verify_base32(&base32(&secret), &code, now, 0), Some(step));
    }

    #[test]
    fn lockouts_escalate_to_a_day() {
        assert_eq!(lockout_secs(1), 15 * 60);
        assert_eq!(lockout_secs(2), 30 * 60);
        assert_eq!(lockout_secs(3), 60 * 60);
        assert_eq!(lockout_secs(7), 16 * 3600);
        assert_eq!(lockout_secs(8), 24 * 3600);
        assert_eq!(lockout_secs(40), 24 * 3600);
    }

    #[test]
    fn an_older_stores_day_long_user_lockout_is_not_honoured() {
        // Written before per-source lockouts: the whole user, for a day.
        let t: TotpFailures = serde_json::from_str(
            r#"{"count":2,"locked_until":1800086400,"level":8,"since_sign_in":40}"#,
        )
        .unwrap();
        assert_eq!(locked_until(&t, "192.0.2.1", 1_800_000_000), None);
        assert_eq!(t.since_sign_in, 40, "the notice still counts them");
    }

    #[test]
    fn sources_are_bounded_and_trusted_ones_kept() {
        let mut t = TotpFailures::default();
        record_success(&mut t, "home", 1);
        for i in 0..(MAX_SOURCES * 3) {
            record_failure(&mut t, &format!("10.0.0.{i}"), 10 + i as u64);
        }
        assert_eq!(t.sources.len(), MAX_SOURCES);
        assert!(t.sources.iter().any(|s| s.key == "home" && s.trusted));
    }

    #[test]
    fn decoys_are_bounded() {
        let mut d = Decoys::default();
        for i in 0..(MAX_DECOYS + 10) {
            let _ = d.attempt(&format!("n{i}"), "k", i as u64);
        }
        assert_eq!(d.len(), MAX_DECOYS);
    }

    #[test]
    fn the_uri_and_the_qr_code() {
        let uri = otpauth_uri(&new_secret(), "node.example.test", "W1ABC").unwrap();
        assert!(uri.starts_with("otpauth://totp/"), "{uri}");
        assert!(uri.contains("issuer=astar%20node.example.test"), "{uri}");
        let rows = qr_rows(&uri);
        assert!(rows.len() >= 21 && rows.iter().all(|r| r.len() == rows.len()));
    }
}
