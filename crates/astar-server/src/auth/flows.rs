// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The ceremonies and management routes under `/auth/`: invites, WebAuthn
//! registration and sign-in.
//!
//! Every ceremony is two calls. `begin` checks that sign-in is possible
//! here, counts against the per-IP limit, stores webauthn-rs's state in the
//! ceremony table and hands the browser the options plus a ceremony cookie.
//! `finish` takes the state back by that cookie — once — and, on success,
//! makes every change in a single store write.

use std::sync::Arc;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;
use webauthn_rs::prelude::{
    AuthenticationResult, CredentialID, DiscoverableKey, Passkey, PublicKeyCredential,
    RegisterPublicKeyCredential, SecurityKey, Webauthn, WebauthnError,
};

use crate::http::HttpResponse;

use super::ceremony::{Ceremony, Enrol, KeyAuth, Kind, Reg, TTL_SECS};
use super::routes::check_signin_possible;
use super::service::{AuthService, Principal, Refusal, Req};
use super::session;
use super::store::{
    AuditEntry, AuthData, CONSOLE_CREATOR, Credential, CredentialKind, Invite, InviteTarget, Role,
    TotpFailures, User,
};
use super::webauthn::{origin_of, rp_id_for};
use super::{aaguid, ok_json};

/// The longest an invite may live: seven days.
pub const INVITE_MAX_SECS: u64 = 7 * 86_400;
/// The shortest: five minutes.
pub const INVITE_MIN_SECS: u64 = 300;
// The lifetime check in `invites_create` spells these as float literals.
const _: () = assert!(INVITE_MIN_SECS == 300 && INVITE_MAX_SECS == 604_800);
/// A used, revoked or expired invite stays in the list this long.
pub const INVITE_KEEP_SECS: u64 = 7 * 86_400;

/// Every `/auth/*` route that is not status, me or sign-out.
///
/// # Errors
/// The route's refusal, or 404.
pub fn route(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    if let Some(r) = super::totp::route(svc, req) {
        return r;
    }
    let p = req.path;
    match (req.method, p) {
        ("POST", "/auth/passkey/begin") => passkey_begin(svc, req),
        ("POST", "/auth/passkey/finish") => passkey_finish(svc, req),
        ("POST", "/auth/key/begin") => key_begin(svc, req),
        ("POST", "/auth/key/finish") => key_finish(svc, req),
        ("POST", "/auth/invite/info") => invite_info(svc, req),
        ("POST", "/auth/invite/begin") => invite_begin(svc, req),
        ("POST", "/auth/invite/finish") => invite_finish(svc, req),
        ("GET", "/auth/invites") => invites_list(svc, req),
        ("POST", "/auth/invites") => invites_create(svc, req),
        ("DELETE", _) if p.starts_with("/auth/invites/") => {
            invites_revoke(svc, req, &p["/auth/invites/".len()..])
        }
        _ => super::manage::route(svc, req),
    }
}

// ---- shared pieces ------------------------------------------------------------

/// Parse a JSON body. The parser's own message is not passed on: it quotes
/// the input, and the input may be a token.
pub(crate) fn body<T: DeserializeOwned>(req: &Req<'_>) -> Result<T, Refusal> {
    let bytes = if req.body.is_empty() {
        b"{}".as_slice()
    } else {
        req.body
    };
    serde_json::from_slice(bytes)
        .map_err(|_| Refusal::new(400, "bad_request", "That request was not understood."))
}

/// A signed-in principal, or 401.
pub(crate) fn need_principal(svc: &AuthService, req: &Req<'_>) -> Result<Principal, Refusal> {
    svc.principal(req)
        .ok_or_else(|| Refusal::new(401, "signin_required", "Sign in to do that."))
}

/// A signed-in user (not the console), or 401.
pub(crate) fn need_user(
    svc: &AuthService,
    req: &Req<'_>,
) -> Result<super::service::SessionUser, Refusal> {
    match need_principal(svc, req)? {
        Principal::User(u) => Ok(u),
        Principal::Console => Err(Refusal::new(
            400,
            "console",
            "The console has no account of its own; sign in as a user for that.",
        )),
    }
}

/// An admin principal, or 401/403.
pub(crate) fn need_admin(svc: &AuthService, req: &Req<'_>) -> Result<Principal, Refusal> {
    let p = need_principal(svc, req)?;
    if p.is_admin() {
        Ok(p)
    } else {
        Err(Refusal::new(
            403,
            "admins_only",
            "Only an admin can do that.",
        ))
    }
}

/// A credential used within the step-up window, or 401 `reauth_required`.
pub(crate) fn need_step_up(svc: &AuthService, p: &Principal) -> Result<(), Refusal> {
    if p.stepped_up(svc.now(), svc.settings.step_up_secs) {
        Ok(())
    } else {
        Err(Refusal::new(
            401,
            "reauth_required",
            "Confirm it's you to do that.",
        ))
    }
}

/// Sign-in is possible here, and this IP has `begin`s left.
pub(crate) fn begin_checks(svc: &AuthService, req: &Req<'_>) -> Result<(), Refusal> {
    check_signin_possible(svc, req)?;
    let now = svc.now();
    if !svc
        .limits
        .lock()
        .expect("limits poisoned")
        .allow_begin(&req.client_key(), now)
    {
        return Err(Refusal::new(
            429,
            "too_many",
            "Too many attempts from here; wait a minute and try again.",
        ));
    }
    Ok(())
}

/// The RP ID and the `Webauthn` for this request's host.
pub(crate) fn webauthn_for(
    svc: &AuthService,
    req: &Req<'_>,
) -> Result<(String, Arc<Webauthn>), Refusal> {
    let host = req.hostname();
    let rp = rp_id_for(svc.settings.rp_id.as_deref(), &host);
    let w = svc.webauthn.get(&rp, &origin_of(req))?;
    Ok((rp, w))
}

/// Store a ceremony and answer with `body` and the ceremony cookie.
pub(crate) fn with_ceremony(
    svc: &AuthService,
    req: &Req<'_>,
    kind: Kind,
    body: &Value,
) -> HttpResponse {
    let now = svc.now();
    let id = svc.ceremonies.lock().expect("ceremonies poisoned").insert(
        Ceremony {
            created: now,
            host: req.hostname(),
            client: req.client_key(),
            kind,
        },
        now,
    );
    ok_json(body).with_header(
        "Set-Cookie",
        &session::set_cookie(
            session::ceremony_cookie(req.tls),
            &id,
            req.tls,
            Some(TTL_SECS),
        ),
    )
}

fn ceremony_gone() -> Refusal {
    Refusal::new(
        400,
        "ceremony_expired",
        "That took too long, or was started in another window. Try again.",
    )
}

/// Take this browser's ceremony, started on this host.
pub(crate) fn take_ceremony(svc: &AuthService, req: &Req<'_>) -> Result<Ceremony, Refusal> {
    let id = session::cookie_value(req.cookie, session::ceremony_cookie(req.tls))
        .ok_or_else(ceremony_gone)?;
    let now = svc.now();
    let c = svc
        .ceremonies
        .lock()
        .expect("ceremonies poisoned")
        .take(id, now)
        .ok_or_else(ceremony_gone)?;
    if c.host != req.hostname() {
        return Err(ceremony_gone());
    }
    Ok(c)
}

/// What a successful sign-in hands back.
pub(crate) struct SignedIn {
    token: String,
    name: String,
    role: Role,
    failed_codes: u32,
}

/// Inside a store write: start a session for `user_id`, signed in with
/// the credential `credential`, note the sign-in, and forgive this
/// source's TOTP failures (any successful credential does).
pub(crate) fn sign_in_within(
    svc: &AuthService,
    d: &mut AuthData,
    req: &Req<'_>,
    user_id: Uuid,
    method: &str,
    credential: Option<String>,
) -> Result<SignedIn, Refusal> {
    let now = svc.now();
    let user = d.user_mut(user_id).ok_or_else(unknown_user)?;
    let failed_codes = user.totp.since_sign_in;
    user.last_sign_in = Some(now);
    super::totp::record_success(&mut user.totp, &req.client_key(), now);
    let (name, role) = (user.name.clone(), user.role);
    d.audit(AuditEntry {
        at: now,
        event: "sign_in".into(),
        user: Some(name.clone()),
        detail: method.to_string(),
        ip: req.ip(),
        times: 0,
        since: None,
    });
    let token = svc.new_session(d, user_id, credential, req);
    Ok(SignedIn {
        token,
        name,
        role,
        failed_codes,
    })
}

/// The answer to a successful sign-in: the session cookie set, the
/// ceremony cookie cleared.
pub(crate) fn signed_in_response(svc: &AuthService, req: &Req<'_>, s: &SignedIn) -> HttpResponse {
    ok_json(&json!({
        "ok": true,
        "name": s.name,
        "role": s.role.as_str(),
        "failed_codes": s.failed_codes,
    }))
    .with_header("Set-Cookie", &svc.session_set_cookie(&s.token, req.tls))
    .with_header(
        "Set-Cookie",
        &session::clear_cookie(session::ceremony_cookie(req.tls), req.tls),
    )
}

fn unknown_user() -> Refusal {
    Refusal::new(404, "unknown_user", "That user no longer exists.")
}

/// Record a failed WebAuthn sign-in, as a counter regression when that is
/// what it was, and return `refusal`.
fn failed(
    svc: &AuthService,
    req: &Req<'_>,
    user: Option<Uuid>,
    error: &WebauthnError,
    refusal: Refusal,
) -> Refusal {
    let name = user.and_then(|u| svc.read(|d| d.user(u).map(|x| x.name.clone())));
    if matches!(error, WebauthnError::CredentialPossibleCompromise) {
        let nick = user
            .and_then(|u| {
                svc.read(|d| {
                    d.credentials_of(u)
                        .find(|c| c.kind.rp_id().is_some())
                        .map(|c| c.nickname.clone())
                })
            })
            .unwrap_or_default();
        svc.audit(
            "counter_regression",
            name.as_deref(),
            &format!("a key's signature counter went backwards ({nick}): it may have been cloned"),
            &req.ip(),
        );
        return Refusal::new(
            401,
            "counter_regression",
            "That key's counter went backwards, which can mean it was copied. It was refused; use another way in.",
        );
    }
    svc.audit_failure("sign_in_failed", name.as_deref(), "webauthn", req);
    refusal
}

/// Apply an authentication's counter and backup-state update to the
/// credential it used.
pub(crate) fn apply_result(d: &mut AuthData, result: &AuthenticationResult, now: u64) {
    let used: &[u8] = result.cred_id().as_ref();
    for c in &mut d.credentials {
        if c.kind.webauthn_id() != Some(used) {
            continue;
        }
        c.last_used = Some(now);
        match &mut c.kind {
            CredentialKind::Passkey {
                passkey,
                backup_state,
                ..
            } => {
                passkey.update_credential(result);
                *backup_state = result.backup_state();
            }
            CredentialKind::SecurityKey { key, .. } => {
                key.update_credential(result);
            }
            CredentialKind::Totp { .. } => {}
        }
    }
}

/// The id of the credential an authentication used.
fn credential_by_webauthn_id(d: &AuthData, result: &AuthenticationResult) -> Option<String> {
    let used: &[u8] = result.cred_id().as_ref();
    d.credentials
        .iter()
        .find(|c| c.kind.webauthn_id() == Some(used))
        .map(|c| c.id.clone())
}

/// A user's WebAuthn credentials on one RP ID.
pub(crate) fn keys_of(d: &AuthData, user: Uuid, rp: &str) -> (Vec<Passkey>, Vec<SecurityKey>) {
    let mut passkeys = Vec::new();
    let mut keys = Vec::new();
    for c in d.credentials_of(user) {
        match &c.kind {
            CredentialKind::Passkey { rp_id, passkey, .. } if rp_id == rp => {
                passkeys.push(passkey.clone());
            }
            CredentialKind::SecurityKey { rp_id, key, .. } if rp_id == rp => {
                keys.push(key.clone());
            }
            _ => {}
        }
    }
    (passkeys, keys)
}

/// Whether `name` can be a user name: 1–32 letters, digits, `-`, `_`,
/// `.` or `/`. A callsign fits; so does a short handle.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
}

/// A nickname or display name as stored: trimmed, control characters
/// dropped, at most 64 characters.
#[must_use]
pub fn clean_text(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect()
}

// ---- discoverable sign-in (the passkey button) ---------------------------------

fn passkey_begin(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    begin_checks(svc, req)?;
    let (_, w) = webauthn_for(svc, req)?;
    let (mut rcr, state) = w
        .start_discoverable_authentication()
        .map_err(|_| internal())?;
    // webauthn-rs forces conditional mediation for autofill; the button is
    // an ordinary, modal request. The page sets mediation itself for the
    // autofill request it makes on load.
    rcr.mediation = None;
    Ok(with_ceremony(
        svc,
        req,
        Kind::Discoverable(state),
        &json!({ "options": rcr }),
    ))
}

#[derive(Deserialize)]
struct FinishAuth {
    credential: PublicKeyCredential,
}

fn passkey_finish(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let FinishAuth { credential } = body(req)?;
    let c = take_ceremony(svc, req)?;
    let Kind::Discoverable(state) = c.kind else {
        return Err(ceremony_gone());
    };
    let (rp, w) = webauthn_for(svc, req)?;
    let not_here = || {
        Refusal::new(
            401,
            "unknown_credential",
            "That passkey isn't registered on this node.",
        )
    };
    let Ok((user_id, _)) = w.identify_discoverable_authentication(&credential) else {
        svc.audit_failure("sign_in_failed", None, "passkey", req);
        return Err(not_here());
    };
    let (passkeys, _) = svc.read(|d| keys_of(d, user_id, &rp));
    if passkeys.is_empty() {
        svc.audit_failure("sign_in_failed", None, "passkey", req);
        return Err(not_here());
    }
    let dks: Vec<DiscoverableKey> = passkeys.iter().map(DiscoverableKey::from).collect();
    let result = match w.finish_discoverable_authentication(&credential, state, &dks) {
        Ok(r) => r,
        Err(e) => return Err(failed(svc, req, Some(user_id), &e, not_here())),
    };
    let now = svc.now();
    let s = svc.mutate(|d| {
        apply_result(d, &result, now);
        let used = credential_by_webauthn_id(d, &result);
        sign_in_within(svc, d, req, user_id, "passkey", used)
    })?;
    Ok(signed_in_response(svc, req, &s))
}

// ---- identifier-first sign-in (name, then a key) --------------------------------

#[derive(Deserialize)]
struct KeyBegin {
    name: String,
    /// `false`: use the user's security keys without a PIN.
    #[serde(default)]
    pin: Option<bool>,
}

/// Made-up credentials for a name with no key here: webauthn-rs's HMAC
/// generator, keyed per process, so the same name gets the same ids, in a
/// realistic number and length; `label` keeps a persona's passkeys and keys
/// apart. Each gets the transports a browser commonly records for its
/// kind, also drawn from the name.
fn decoy_ids(svc: &AuthService, name: &str, label: &str) -> Vec<Value> {
    use webauthn_rs::fake::{FakePasskeyDistribution, WebauthnFakeCredentialGenerator};
    let picks = decoy_digest(svc, label, name);
    let generator = WebauthnFakeCredentialGenerator::<FakePasskeyDistribution>::new(&svc.decoy_key);
    // The distribution sometimes draws none, and a real user here always
    // has at least one: draw again, still deterministically, until it
    // gives some.
    (0..16)
        .find_map(|draw| {
            let seed = format!("{label}\0{}\0{draw}", name.trim().to_ascii_lowercase());
            generator
                .as_ref()
                .ok()
                .and_then(|g| g.generate(seed.as_bytes()).ok())
                .filter(|ids| !ids.is_empty())
        })
        .map(|ids| {
            ids.iter()
                .enumerate()
                .map(|(i, id)| {
                    let transports: &[&str] = match (label, picks[1 + i % 31] % 3) {
                        ("key", 0) => &["usb"],
                        ("key", 1) => &["nfc", "usb"],
                        ("key", _) => &["usb", "nfc"],
                        (_, 0) => &[],
                        (_, 1) => &["internal"],
                        (_, _) => &["hybrid", "internal"],
                    };
                    let mut c = json!({ "type": "public-key", "id": id });
                    if !transports.is_empty() {
                        c["transports"] = json!(transports);
                    }
                    c
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What a made-up user for an unknown name holds: only passkeys, only
/// keys without a PIN, or both — drawn from a hash of the name keyed per
/// process, so one name always gets the same answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DecoyPersona {
    Passkeys,
    Both,
    Keys,
}

/// SHA-256 of the per-process key, a label and the name as sign-in
/// matches it.
fn decoy_digest(svc: &AuthService, label: &str, name: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(svc.decoy_key);
    h.update(label.as_bytes());
    h.update([0]);
    h.update(name.trim().to_ascii_lowercase().as_bytes());
    h.finalize().into()
}

/// The persona an unknown name is answered as: mostly passkeys.
pub(crate) fn decoy_persona(svc: &AuthService, name: &str) -> DecoyPersona {
    match decoy_digest(svc, "persona", name)[0] % 20 {
        0..14 => DecoyPersona::Passkeys,
        14..17 => DecoyPersona::Both,
        _ => DecoyPersona::Keys,
    }
}

fn key_begin(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let b: KeyBegin = body(req)?;
    begin_checks(svc, req)?;
    let (rp, w) = webauthn_for(svc, req)?;
    let found = svc.read(|d| {
        d.user_by_name(&b.name).map(|u| {
            let (p, k) = keys_of(d, u.id, &rp);
            (u.id, p, k)
        })
    });
    let (kind, rcr, no_pin_keys) = match found {
        Some((user_id, passkeys, keys))
            if !keys.is_empty() && (b.pin == Some(false) || passkeys.is_empty()) =>
        {
            let (rcr, st) = w
                .start_securitykey_authentication(&keys)
                .map_err(|_| internal())?;
            (
                Kind::SignIn {
                    user_id,
                    auth: KeyAuth::SecurityKey(st),
                },
                rcr,
                true,
            )
        }
        Some((user_id, passkeys, keys)) if !passkeys.is_empty() => {
            let (rcr, st) = w
                .start_passkey_authentication(&passkeys)
                .map_err(|_| internal())?;
            (
                Kind::SignIn {
                    user_id,
                    auth: KeyAuth::Passkey(st),
                },
                rcr,
                !keys.is_empty(),
            )
        }
        _ => {
            // Nobody by that name, or nobody with a key here: answer as a
            // made-up user would be answered, by the same rules as above.
            let persona = decoy_persona(svc, &b.name);
            let by_key = persona == DecoyPersona::Keys
                || (persona == DecoyPersona::Both && b.pin == Some(false));
            let (mut rcr, _) = w
                .start_discoverable_authentication()
                .map_err(|_| internal())?;
            rcr.mediation = None;
            let mut options = serde_json::to_value(&rcr).map_err(|_| internal())?;
            let pk = &mut options["publicKey"];
            // What webauthn-rs's passkey and security-key requests carry,
            // and its discoverable one does not, or the other way round.
            if let Some(o) = pk.as_object_mut() {
                o.remove("extensions");
                o.remove("hints");
            }
            if by_key {
                pk["userVerification"] = json!("preferred");
                pk["hints"] = json!(["security-key"]);
            } else {
                pk["userVerification"] = json!("required");
            }
            pk["allowCredentials"] = Value::Array(decoy_ids(
                svc,
                &b.name,
                if by_key { "key" } else { "passkey" },
            ));
            return Ok(with_ceremony(
                svc,
                req,
                Kind::Decoy,
                &json!({ "options": options, "no_pin_keys": persona != DecoyPersona::Passkeys }),
            ));
        }
    };
    Ok(with_ceremony(
        svc,
        req,
        kind,
        &json!({ "options": rcr, "no_pin_keys": no_pin_keys }),
    ))
}

fn key_finish(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let FinishAuth { credential } = body(req)?;
    let c = take_ceremony(svc, req)?;
    let didnt_work = || {
        Refusal::new(
            401,
            "unknown_credential",
            "That key didn't work here. Try again, or use another way in.",
        )
    };
    let (user_id, auth) = match c.kind {
        Kind::SignIn { user_id, auth } => (user_id, auth),
        Kind::Decoy => {
            svc.audit_failure("sign_in_failed", None, "security key", req);
            return Err(didnt_work());
        }
        _ => return Err(ceremony_gone()),
    };
    let (_, w) = webauthn_for(svc, req)?;
    let (outcome, method) = match &auth {
        KeyAuth::Passkey(st) => (w.finish_passkey_authentication(&credential, st), "passkey"),
        KeyAuth::SecurityKey(st) => (
            w.finish_securitykey_authentication(&credential, st),
            "security key",
        ),
    };
    let result = match outcome {
        Ok(r) => r,
        Err(e) => return Err(failed(svc, req, Some(user_id), &e, didnt_work())),
    };
    let now = svc.now();
    let s = svc.mutate(|d| {
        apply_result(d, &result, now);
        let used = credential_by_webauthn_id(d, &result);
        sign_in_within(svc, d, req, user_id, method, used)
    })?;
    Ok(signed_in_response(svc, req, &s))
}

// ---- invites -------------------------------------------------------------------

/// Why an invite cannot be used, or `Ok` with it.
pub(crate) fn live_invite<'a>(
    d: &'a AuthData,
    hash: &str,
    now: u64,
) -> Result<&'a Invite, Refusal> {
    let inv = d
        .invites
        .iter()
        .find(|i| i.hash == hash)
        .ok_or_else(|| Refusal::new(404, "invite_unknown", "This invite link isn't valid."))?;
    if inv.used_at.is_some() {
        return Err(Refusal::new(
            410,
            "invite_used",
            "This invite has already been used. Ask for a new one.",
        ));
    }
    if inv.revoked || !d.creator_vouches(inv) {
        return Err(Refusal::new(
            410,
            "invite_revoked",
            "This invite was withdrawn. Ask for a new one.",
        ));
    }
    if inv.expires <= now {
        return Err(Refusal::new(
            410,
            "invite_expired",
            "This invite has expired. Ask for a new one.",
        ));
    }
    Ok(inv)
}

/// The store hash of an invite token from a request body, or the refusal
/// an unknown invite gets.
fn token_hash(token: &str) -> Result<String, Refusal> {
    if session::is_token(token) {
        Ok(session::hash_token(token))
    } else {
        Err(Refusal::new(
            404,
            "invite_unknown",
            "This invite link isn't valid.",
        ))
    }
}

/// A new invite record. The CLI and the API both make invites through
/// this, so the rules are one.
#[must_use]
pub fn invite_record(
    hash: String,
    target: InviteTarget,
    name: Option<String>,
    created_by: &str,
    created_by_id: Uuid,
    now: u64,
    lifetime: u64,
) -> Invite {
    Invite {
        id: session::new_id(),
        hash,
        target,
        name,
        created_by: created_by.to_string(),
        created_by_id: Some(created_by_id),
        created: now,
        expires: now + lifetime.clamp(INVITE_MIN_SECS, INVITE_MAX_SECS),
        used_at: None,
        used_by: None,
        revoked: false,
    }
}

/// Drop invites that ended (used, revoked or expired) more than a week ago.
pub(crate) fn prune_invites(d: &mut AuthData, now: u64) {
    d.invites.retain(|i| {
        let ended = i.used_at.unwrap_or(if i.revoked || i.expires <= now {
            i.expires.min(now)
        } else {
            u64::MAX
        });
        ended == u64::MAX || now.saturating_sub(ended) <= INVITE_KEEP_SECS
    });
}

/// The default lifetime for an invite to `target`.
#[must_use]
pub fn default_lifetime(target: &InviteTarget) -> u64 {
    match target {
        InviteTarget::NewUser { role: Role::Viewer } => 24 * 3600,
        InviteTarget::NewUser { role: Role::Admin } | InviteTarget::User { .. } => 3600,
    }
}

/// The role whoever redeems `i` ends up with: the new user's, or the
/// re-enrolled user's as it is now.
pub(crate) fn invite_role(d: &AuthData, i: &Invite) -> Role {
    match &i.target {
        InviteTarget::NewUser { role } => *role,
        InviteTarget::User { user_id } => d.user(*user_id).map_or(Role::Viewer, |u| u.role),
    }
}

/// An authenticator app offered where only a passkey or key will do.
pub(crate) fn key_required() -> Refusal {
    Refusal::new(
        403,
        "key_required",
        "Admins sign in with a passkey or security key, not an authenticator app. Choose one of those; you can add an app afterwards to confirm it's you.",
    )
}

/// An invite as the API lists it. No token and no hash.
fn invite_json(d: &AuthData, i: &Invite, now: u64) -> Value {
    let state = if i.used_at.is_some() {
        "used"
    } else if i.revoked || !d.creator_vouches(i) {
        "revoked"
    } else if i.expires <= now {
        "expired"
    } else {
        "pending"
    };
    let (role, for_user) = match &i.target {
        InviteTarget::NewUser { role } => (role.as_str(), None),
        InviteTarget::User { user_id } => (
            d.user(*user_id).map_or("viewer", |u| u.role.as_str()),
            d.user(*user_id).map(|u| u.name.clone()),
        ),
    };
    json!({
        "id": i.id,
        "role": role,
        "for_user": for_user,
        "name": i.name,
        "created_by": i.created_by,
        "created": i.created,
        "expires": i.expires,
        "used_at": i.used_at,
        "used_by": i.used_by,
        "state": state,
    })
}

#[derive(Deserialize)]
struct CreateInvite {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    hours: Option<f64>,
}

fn invites_create(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let p = need_admin(svc, req)?;
    need_step_up(svc, &p)?;
    let b: CreateInvite = body(req)?;
    let bad = |m: &str| Refusal::new(400, "bad_request", m.to_string());
    let target = match (b.role.as_deref(), b.user_id.as_deref()) {
        (_, Some(uid)) => {
            let user_id = Uuid::parse_str(uid).map_err(|_| bad("No such user."))?;
            if svc.read(|d| d.user(user_id).is_none()) {
                return Err(unknown_user());
            }
            InviteTarget::User { user_id }
        }
        (Some(r), None) => InviteTarget::NewUser {
            role: Role::parse(r).ok_or_else(|| bad("The role is admin or viewer."))?,
        },
        (None, None) => return Err(bad("An invite needs a role, or a user to re-enrol.")),
    };
    let name = b
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    if let Some(n) = &name
        && !valid_name(n)
    {
        return Err(bad(
            "A name is 1 to 32 letters, digits, '-', '_', '.' or '/'.",
        ));
    }
    let lifetime = match b.hours {
        None => default_lifetime(&target),
        Some(h) if h.is_finite() && h > 0.0 => {
            // Whole seconds; the bounds are checked before the cast.
            let secs = (h * 3600.0).round();
            if !(300.0..=604_800.0).contains(&secs) {
                return Err(bad("An invite lasts between 5 minutes and 7 days."));
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "checked to lie within 300..=604800 (INVITE_MIN_SECS..=INVITE_MAX_SECS) above"
            )]
            let secs = secs as u64;
            secs
        }
        Some(_) => return Err(bad("An invite lasts between 5 minutes and 7 days.")),
    };
    let token = session::new_token();
    let now = svc.now();
    let creator = p.audit_name();
    let ip = req.ip();
    let inv = invite_record(
        session::hash_token(&token),
        target,
        name,
        &creator,
        match &p {
            Principal::Console => CONSOLE_CREATOR,
            Principal::User(u) => u.user_id,
        },
        now,
        lifetime,
    );
    let listed = svc.mutate(|d| {
        prune_invites(d, now);
        let listed = invite_json(d, &inv, now);
        let detail = format!(
            "{} invite{}",
            listed["role"].as_str().unwrap_or_default(),
            listed["for_user"]
                .as_str()
                .map(|u| format!(" to re-enrol {u}"))
                .unwrap_or_default()
        );
        d.invites.push(inv);
        d.audit(AuditEntry {
            at: now,
            event: "invite_create".into(),
            user: Some(creator.clone()),
            detail,
            ip,
            times: 0,
            since: None,
        });
        Ok(listed)
    })?;
    let mut out = listed;
    // The token leaves the server exactly once, here, in the link (and the
    // QR code of it, for enrolling a phone).
    let link = format!("{}/admin/invite#{token}", origin_of(req));
    out["qr"] = json!(super::totp::qr_rows(&link));
    out["link"] = Value::String(link);
    Ok(ok_json(&out))
}

fn invites_list(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    need_admin(svc, req)?;
    let now = svc.now();
    let list = svc.read(|d| {
        let mut v: Vec<&Invite> = d
            .invites
            .iter()
            .filter(|i| {
                let ended = i
                    .used_at
                    .or((i.revoked || i.expires <= now).then_some(i.expires));
                ended.is_none_or(|t| now.saturating_sub(t) <= INVITE_KEEP_SECS)
            })
            .collect();
        v.sort_by_key(|i| std::cmp::Reverse(i.created));
        v.iter().map(|i| invite_json(d, i, now)).collect::<Vec<_>>()
    });
    Ok(ok_json(&json!({ "invites": list })))
}

fn invites_revoke(svc: &AuthService, req: &Req<'_>, id: &str) -> Result<HttpResponse, Refusal> {
    let p = need_admin(svc, req)?;
    let now = svc.now();
    let who = p.audit_name();
    let ip = req.ip();
    svc.mutate(|d| {
        let inv = d
            .invites
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| Refusal::new(404, "not_found", "No such invite."))?;
        if inv.used_at.is_none() {
            inv.revoked = true;
        }
        d.audit(AuditEntry {
            at: now,
            event: "invite_revoke".into(),
            user: Some(who.clone()),
            detail: String::new(),
            ip,
            times: 0,
            since: None,
        });
        Ok(())
    })?;
    Ok(ok_json(&json!({ "ok": true })))
}

#[derive(Deserialize)]
struct TokenBody {
    token: String,
}

fn invite_info(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let TokenBody { token } = body(req)?;
    let hash = token_hash(&token)?;
    let now = svc.now();
    let v = svc.read(|d| {
        let inv = live_invite(d, &hash, now)?;
        let mut v = invite_json(d, inv, now);
        v["node"] = json!(svc.settings.node);
        v["inviter"] = Value::String(inv.created_by.clone());
        // An admin enrols a passkey or key; an app cannot sign them in.
        v["app_allowed"] = json!(invite_role(d, inv) != Role::Admin);
        Ok::<Value, Refusal>(v)
    })?;
    Ok(ok_json(&v))
}

#[derive(Deserialize)]
struct InviteBegin {
    token: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    /// `"passkey"` or `"security_key"` (a key without a PIN).
    kind: String,
}

fn invite_begin(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let b: InviteBegin = body(req)?;
    let hash = token_hash(&b.token)?;
    begin_checks(svc, req)?;
    let (enrol, user_id, exclude) = enrolment_for_invite(svc, &hash, b.name, b.display_name)?;
    let (_, w) = webauthn_for(svc, req)?;
    let Enrol::Invite {
        name, display_name, ..
    } = &enrol
    else {
        return Err(internal());
    };
    let (ccr, reg) = start_registration(&w, &b.kind, user_id, name, display_name, exclude)?;
    Ok(with_ceremony(
        svc,
        req,
        Kind::Register {
            enrol,
            user_id,
            reg,
        },
        &json!({ "options": ccr }),
    ))
}

/// Who an invite enrols: a new user under the chosen name, or the existing
/// user it names; with the ids their existing keys must not repeat.
pub(crate) fn enrolment_for_invite(
    svc: &AuthService,
    hash: &str,
    name: Option<String>,
    display_name: Option<String>,
) -> Result<(Enrol, Uuid, Option<Vec<CredentialID>>), Refusal> {
    let now = svc.now();
    svc.read(|d| {
        let inv = live_invite(d, hash, now)?;
        match &inv.target {
            InviteTarget::NewUser { .. } => {
                let name = name
                    .or_else(|| inv.name.clone())
                    .map(|n| n.trim().to_string())
                    .unwrap_or_default();
                if !valid_name(&name) {
                    return Err(Refusal::new(
                        400,
                        "bad_name",
                        "Choose a name: 1 to 32 letters, digits, '-', '_', '.' or '/'. Your callsign is ideal.",
                    ));
                }
                if d.user_by_name(&name).is_some() {
                    return Err(name_taken());
                }
                let display = display_name
                    .map(|n| clean_text(&n))
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| name.clone());
                Ok((
                    Enrol::Invite {
                        invite_hash: hash.to_string(),
                        name,
                        display_name: display,
                    },
                    Uuid::new_v4(),
                    None,
                ))
            }
            InviteTarget::User { user_id } => {
                let u = d.user(*user_id).ok_or_else(unknown_user)?;
                Ok((
                    Enrol::Invite {
                        invite_hash: hash.to_string(),
                        name: u.name.clone(),
                        display_name: u.display_name.clone(),
                    },
                    u.id,
                    Some(existing_ids(d, u.id)),
                ))
            }
        }
    })
}

fn name_taken() -> Refusal {
    Refusal::new(
        409,
        "name_taken",
        "That name is taken on this node; choose another.",
    )
}

/// Every WebAuthn credential id a user has: the exclude list, so the same
/// key cannot be enrolled twice.
pub(crate) fn existing_ids(d: &AuthData, user: Uuid) -> Vec<CredentialID> {
    d.credentials_of(user)
        .filter_map(|c| {
            c.kind
                .webauthn_id()
                .map(|id| CredentialID::from(id.to_vec()))
        })
        .collect()
}

/// Start a registration of one of three kinds, and the options for the
/// browser as JSON:
///
/// * `passkey` — user verification required, and a discoverable credential
///   preferred, so the "Sign in with a passkey" button can find it. Phones,
///   computers and password managers make one anyway; the preference is
///   what keeps a virtual or older platform authenticator in line.
/// * `pin_key` — a FIDO2 key with a PIN: the same verification, but a
///   non-resident credential, which spares the key's limited slots. It is
///   used by name (identifier-first).
/// * `security_key` — a key without a PIN: possession only.
///
/// The resident-key preference is advice to the browser; webauthn-rs
/// verifies the result the same way whichever the authenticator chose.
pub(crate) fn start_registration(
    w: &Webauthn,
    kind: &str,
    user_id: Uuid,
    name: &str,
    display: &str,
    exclude: Option<Vec<CredentialID>>,
) -> Result<(Value, Reg), Refusal> {
    let (ccr, reg) = match kind {
        "passkey" | "pin_key" => w
            .start_passkey_registration(user_id, name, display, exclude)
            .map(|(c, r)| (c, Reg::Passkey(r)))
            .map_err(|_| internal())?,
        "security_key" => w
            .start_securitykey_registration(user_id, name, display, exclude, None, None)
            .map(|(c, r)| (c, Reg::SecurityKey(r)))
            .map_err(|_| internal())?,
        _ => {
            return Err(Refusal::new(
                400,
                "bad_request",
                "The kind is passkey, pin_key or security_key.",
            ));
        }
    };
    let mut options = serde_json::to_value(&ccr).map_err(|_| internal())?;
    if kind == "passkey" {
        options["publicKey"]["authenticatorSelection"]["residentKey"] = json!("preferred");
        options["publicKey"]["authenticatorSelection"]["requireResidentKey"] = json!(false);
    }
    Ok((options, reg))
}

#[derive(Deserialize)]
pub(crate) struct RegFinish {
    pub credential: RegisterPublicKeyCredential,
    #[serde(default)]
    pub nickname: Option<String>,
}

/// Finish a WebAuthn registration into a credential record.
pub(crate) fn finish_registration(
    svc: &AuthService,
    req: &Req<'_>,
    reg: &Reg,
    user_id: Uuid,
    finish: &RegFinish,
) -> Result<Credential, Refusal> {
    let (rp, w) = webauthn_for(svc, req)?;
    let not_made = || {
        Refusal::new(
            400,
            "registration_failed",
            "That didn't register. Try again; if it keeps failing, try another kind of credential.",
        )
    };
    let info =
        aaguid::from_attestation_object(finish.credential.response.attestation_object.as_ref());
    let model = info.aaguid.as_deref().and_then(aaguid::label);
    let (kind, generic) = match reg {
        Reg::Passkey(st) => {
            let pk = w
                .finish_passkey_registration(&finish.credential, st)
                .map_err(|_| not_made())?;
            (
                CredentialKind::Passkey {
                    rp_id: rp,
                    passkey: pk,
                    aaguid: info.aaguid.clone(),
                    backup_eligible: info.backup_eligible,
                    backup_state: info.backup_state,
                },
                "Passkey",
            )
        }
        Reg::SecurityKey(st) => {
            let key = w
                .finish_securitykey_registration(&finish.credential, st)
                .map_err(|_| not_made())?;
            (
                CredentialKind::SecurityKey {
                    rp_id: rp,
                    key,
                    aaguid: info.aaguid.clone(),
                },
                "Security key",
            )
        }
    };
    let nickname = finish
        .nickname
        .as_deref()
        .map(clean_text)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| model.unwrap_or(generic).to_string());
    Ok(Credential {
        id: session::new_id(),
        user_id,
        nickname,
        created: svc.now(),
        last_used: None,
        kind,
    })
}

/// Inside a store write: a credential id nobody has yet.
pub(crate) fn check_new_credential(d: &AuthData, c: &Credential) -> Result<(), Refusal> {
    if let Some(id) = c.kind.webauthn_id()
        && d.credentials
            .iter()
            .any(|x| x.kind.webauthn_id() == Some(id))
    {
        return Err(Refusal::new(
            409,
            "already_registered",
            "That key is already registered on this node.",
        ));
    }
    Ok(())
}

/// Inside a store write: consume the invite, create or extend the user,
/// save the credential and sign in — all or nothing.
pub(crate) fn redeem_within(
    svc: &AuthService,
    d: &mut AuthData,
    req: &Req<'_>,
    enrol: &Enrol,
    user_id: Uuid,
    cred: Credential,
) -> Result<SignedIn, Refusal> {
    let now = svc.now();
    let Enrol::Invite {
        invite_hash,
        name,
        display_name,
    } = enrol
    else {
        return Err(internal());
    };
    let inv = live_invite(d, invite_hash, now)?;
    let target = inv.target.clone();
    // Checked here, in the write that saves it, so a promotion while the
    // app was being set up cannot leave an admin enrolled with only codes.
    if matches!(cred.kind, CredentialKind::Totp { .. }) && invite_role(d, inv) == Role::Admin {
        return Err(key_required());
    }
    check_new_credential(d, &cred)?;
    match target {
        InviteTarget::NewUser { role } => {
            if d.user_by_name(name).is_some() {
                return Err(name_taken());
            }
            d.users.push(User {
                id: user_id,
                name: name.clone(),
                display_name: display_name.clone(),
                role,
                created: now,
                last_sign_in: None,
                totp: TotpFailures::default(),
            });
        }
        InviteTarget::User { user_id: target } => {
            if target != user_id || d.user(user_id).is_none() {
                return Err(unknown_user());
            }
        }
    }
    let nickname = cred.nickname.clone();
    let kind = cred.kind.name();
    let cred_id = cred.id.clone();
    d.credentials.push(cred);
    if let Some(inv) = d.invites.iter_mut().find(|i| i.hash == *invite_hash) {
        inv.used_at = Some(now);
        inv.used_by = Some(name.clone());
    }
    let ip = req.ip();
    d.audit(AuditEntry {
        at: now,
        event: "invite_use".into(),
        user: Some(name.clone()),
        detail: String::new(),
        ip: ip.clone(),
        times: 0,
        since: None,
    });
    d.audit(AuditEntry {
        at: now,
        event: "credential_add".into(),
        user: Some(name.clone()),
        detail: format!("{kind}: {nickname}"),
        ip,
        times: 0,
        since: None,
    });
    sign_in_within(svc, d, req, user_id, "invite", Some(cred_id))
}

fn invite_finish(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let finish: RegFinish = body(req)?;
    let c = take_ceremony(svc, req)?;
    let Kind::Register {
        enrol: enrol @ Enrol::Invite { .. },
        user_id,
        reg,
    } = c.kind
    else {
        return Err(ceremony_gone());
    };
    let cred = finish_registration(svc, req, &reg, user_id, &finish)?;
    let s = svc.mutate(|d| redeem_within(svc, d, req, &enrol, user_id, cred))?;
    Ok(signed_in_response(svc, req, &s))
}

/// Add a credential to the signed-in user whose session made the
/// ceremony, and answer with it.
pub(crate) fn ok_after_add(
    svc: &AuthService,
    req: &Req<'_>,
    session_id: &str,
    cred: Credential,
) -> Result<HttpResponse, Refusal> {
    let now = svc.now();
    let ip = req.ip();
    let summary = svc.mutate(|d| {
        let owner = d
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| s.user_id)
            .ok_or_else(|| Refusal::new(401, "signin_required", "Sign in to do that."))?;
        if owner != cred.user_id {
            return Err(Refusal::new(403, "forbidden", "That isn't your account."));
        }
        check_new_credential(d, &cred)?;
        let name = d.user(owner).map(|u| u.name.clone());
        let summary = json!({ "id": cred.id, "kind": cred.kind.name(), "nickname": cred.nickname });
        d.audit(AuditEntry {
            at: now,
            event: "credential_add".into(),
            user: name,
            detail: format!("{}: {}", cred.kind.name(), cred.nickname),
            ip,
            times: 0,
            since: None,
        });
        d.credentials.push(cred);
        Ok(summary)
    })?;
    Ok(
        ok_json(&json!({ "ok": true, "credential": summary })).with_header(
            "Set-Cookie",
            &session::clear_cookie(session::ceremony_cookie(req.tls), req.tls),
        ),
    )
}

pub(crate) fn internal() -> Refusal {
    Refusal::new(
        500,
        "internal",
        "Something went wrong on the node. Try again.",
    )
}
