// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Managing what exists: step-up, your credentials and sessions, other
//! users, and the audit.
//!
//! Adding or removing a credential, re-enrolling yourself, and every
//! change to another user need a credential used within the step-up
//! window; renaming does not. The last admin cannot be demoted or deleted,
//! and nobody can be left without a credential. An admin always keeps a
//! passkey or key, since codes cannot sign them in: nobody without one is
//! promoted, and an admin's last one cannot be removed.

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::http::HttpResponse;

use super::ceremony::{Enrol, KeyAuth, Kind};
use super::flows::{
    RegFinish, apply_result, begin_checks, body, existing_ids, finish_registration, internal,
    invite_record, keys_of, need_admin, need_step_up, need_user, ok_after_add, start_registration,
    take_ceremony, webauthn_for, with_ceremony,
};
use super::ok_json;
use super::service::{AuthService, Principal, Refusal, Req, remove_user};
use super::session;
use super::store::{AuditEntry, AuthData, CredentialKind, InviteTarget, Role};
use super::webauthn::{origin_of, rp_id_for};

/// A self-service re-enrol invite lives this long.
pub const REENROL_SECS: u64 = 15 * 60;

/// Inside a store write: a credential was just used in this session.
pub(crate) fn mark_stepped_up(d: &mut AuthData, session_id: &str, now: u64) {
    if let Some(s) = d.sessions.iter_mut().find(|s| s.id == session_id) {
        s.last_auth_at = now;
    }
}

/// The management routes.
///
/// # Errors
/// The route's refusal, or 404.
pub fn route(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let rest = req.path.strip_prefix("/auth/").unwrap_or_default();
    let seg: Vec<&str> = rest.split('/').collect();
    match (req.method, seg.as_slice()) {
        ("POST", ["stepup", "begin"]) => step_up_begin(svc, req),
        ("POST", ["stepup", "finish"]) => step_up_finish(svc, req),
        ("GET", ["credentials"]) => credentials_list(svc, req),
        ("POST", ["credentials", "webauthn", "begin"]) => add_webauthn_begin(svc, req),
        ("POST", ["credentials", "webauthn", "finish"]) => add_webauthn_finish(svc, req),
        ("POST", ["credentials", id, "rename"]) => credential_rename(svc, req, id),
        ("DELETE", ["credentials", id]) => credential_remove(svc, req, id),
        ("GET", ["sessions"]) => sessions_list(svc, req),
        ("DELETE", ["sessions", id]) => session_revoke(svc, req, id),
        ("POST", ["sessions", "others", "revoke"]) => sessions_revoke_others(svc, req),
        ("POST", ["reenrol"]) => reenrol(svc, req),
        ("GET", ["users"]) => users_list(svc, req),
        ("POST", ["users", id, "role"]) => user_role(svc, req, id),
        ("DELETE", ["users", id]) => user_delete(svc, req, id),
        ("POST", ["users", id, "signout"]) => user_sign_out(svc, req, id),
        ("DELETE", ["users", id, "credentials", cid]) => user_credential_remove(svc, req, id, cid),
        ("GET", ["audit"]) => audit(svc, req),
        ("POST", ["cli", _]) => super::cli::server_route(svc, req),
        _ => Err(Refusal::new(404, "not_found", "not found")),
    }
}

fn not_found(what: &str) -> Refusal {
    Refusal::new(404, "not_found", format!("No such {what}."))
}

fn audit_entry(
    svc: &AuthService,
    event: &str,
    user: &str,
    detail: String,
    req: &Req<'_>,
) -> AuditEntry {
    AuditEntry {
        at: svc.now(),
        event: event.to_string(),
        user: Some(user.to_string()),
        detail,
        ip: req.ip(),
        times: 0,
        since: None,
    }
}

// ---- step-up --------------------------------------------------------------------

#[derive(Deserialize)]
struct PinChoice {
    #[serde(default)]
    pin: Option<bool>,
    /// For adding: `passkey` (default), `pin_key` or `security_key`.
    #[serde(default)]
    kind: Option<String>,
}

fn step_up_begin(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    let b: PinChoice = body(req)?;
    begin_checks(svc, req)?;
    let (rp, w) = webauthn_for(svc, req)?;
    let (passkeys, keys) = svc.read(|d| keys_of(d, u.user_id, &rp));
    let (auth, rcr, no_pin_keys) =
        if !keys.is_empty() && (b.pin == Some(false) || passkeys.is_empty()) {
            let (rcr, st) = w
                .start_securitykey_authentication(&keys)
                .map_err(|_| internal())?;
            (KeyAuth::SecurityKey(st), rcr, true)
        } else if !passkeys.is_empty() {
            let (rcr, st) = w
                .start_passkey_authentication(&passkeys)
                .map_err(|_| internal())?;
            (KeyAuth::Passkey(st), rcr, !keys.is_empty())
        } else {
            return Err(Refusal::new(
                400,
                "no_key_here",
                "None of your passkeys or keys works on this hostname; use your authenticator app.",
            ));
        };
    Ok(with_ceremony(
        svc,
        req,
        Kind::StepUp {
            session_id: u.session_id,
            user_id: u.user_id,
            auth,
        },
        &json!({ "options": rcr, "no_pin_keys": no_pin_keys }),
    ))
}

#[derive(Deserialize)]
struct FinishAuth {
    credential: webauthn_rs::prelude::PublicKeyCredential,
}

fn step_up_finish(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    let FinishAuth { credential } = body(req)?;
    let c = take_ceremony(svc, req)?;
    let Kind::StepUp {
        session_id,
        user_id,
        auth,
    } = c.kind
    else {
        return Err(Refusal::new(400, "ceremony_expired", "Try again."));
    };
    if session_id != u.session_id || user_id != u.user_id {
        return Err(Refusal::new(400, "ceremony_expired", "Try again."));
    }
    let (_, w) = webauthn_for(svc, req)?;
    let result = match &auth {
        KeyAuth::Passkey(st) => w.finish_passkey_authentication(&credential, st),
        KeyAuth::SecurityKey(st) => w.finish_securitykey_authentication(&credential, st),
    };
    let Ok(result) = result else {
        svc.audit_failure("step_up_failed", Some(&u.name), "webauthn", req);
        return Err(Refusal::new(
            401,
            "unknown_credential",
            "That key didn't work. Try again, or use another of yours.",
        ));
    };
    let now = svc.now();
    svc.mutate(|d| {
        apply_result(d, &result, now);
        mark_stepped_up(d, &u.session_id, now);
        d.audit(audit_entry(svc, "step_up", &u.name, "webauthn".into(), req));
        Ok(())
    })?;
    Ok(ok_json(&json!({ "ok": true })).with_header(
        "Set-Cookie",
        &session::clear_cookie(session::ceremony_cookie(req.tls), req.tls),
    ))
}

// ---- your credentials -------------------------------------------------------------

/// One credential as the account page shows it. No key material, no
/// secret: the WebAuthn credential id (public, and what the page hands to
/// the browser's `signalUnknownCredential`), the RP ID and the flags.
fn credential_json(
    c: &super::store::Credential,
    this_rp: &str,
    last: bool,
    last_key: bool,
) -> Value {
    let (rp, synced, aaguid, webauthn_id) = match &c.kind {
        CredentialKind::Passkey {
            rp_id,
            aaguid,
            backup_eligible,
            passkey,
            ..
        } => (
            Some(rp_id.as_str()),
            Some(*backup_eligible),
            aaguid.clone(),
            Some(serde_json::to_value(passkey.cred_id()).unwrap_or(Value::Null)),
        ),
        CredentialKind::SecurityKey { rp_id, aaguid, key } => (
            Some(rp_id.as_str()),
            Some(false),
            aaguid.clone(),
            Some(serde_json::to_value(key.cred_id()).unwrap_or(Value::Null)),
        ),
        CredentialKind::Totp { .. } => (None, None, None, None),
    };
    json!({
        "id": c.id,
        "kind": c.kind.name(),
        "nickname": c.nickname,
        "created": c.created,
        "last_used": c.last_used,
        "rp_id": rp,
        "works_here": rp.is_none_or(|r| r == this_rp),
        "synced": synced,
        "aaguid": aaguid,
        "webauthn_id": webauthn_id,
        "last": last,
        // An admin's last passkey or key: it stays while they are one.
        "last_key": last_key,
    })
}

fn this_rp(svc: &AuthService, req: &Req<'_>) -> String {
    rp_id_for(svc.settings.rp_id.as_deref(), &req.hostname())
}

fn credentials_list(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    let rp = this_rp(svc, req);
    let list = svc.read(|d| {
        let mine: Vec<_> = d.credentials_of(u.user_id).collect();
        let last = mine.len() == 1;
        mine.iter()
            .map(|c| credential_json(c, &rp, last, is_last_admin_key(d, u.user_id, c)))
            .collect::<Vec<_>>()
    });
    Ok(ok_json(&json!({ "credentials": list, "rp_id": rp })))
}

fn add_webauthn_begin(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    need_step_up(svc, &Principal::User(u.clone()))?;
    let b: PinChoice = body(req)?;
    begin_checks(svc, req)?;
    let (_, w) = webauthn_for(svc, req)?;
    let (display, exclude) = svc.read(|d| {
        (
            d.user(u.user_id)
                .map(|x| x.display_name.clone())
                .unwrap_or_default(),
            existing_ids(d, u.user_id),
        )
    });
    let kind = match (b.kind.as_deref(), b.pin) {
        (Some(k), _) => k,
        (None, Some(false)) => "security_key",
        (None, _) => "passkey",
    };
    let (ccr, reg) = start_registration(&w, kind, u.user_id, &u.name, &display, Some(exclude))?;
    Ok(with_ceremony(
        svc,
        req,
        Kind::Register {
            enrol: Enrol::Existing {
                session_id: u.session_id,
            },
            user_id: u.user_id,
            reg,
        },
        &json!({ "options": ccr }),
    ))
}

fn add_webauthn_finish(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    let finish: RegFinish = body(req)?;
    let c = take_ceremony(svc, req)?;
    let Kind::Register {
        enrol: Enrol::Existing { session_id },
        user_id,
        reg,
    } = c.kind
    else {
        return Err(Refusal::new(400, "ceremony_expired", "Try again."));
    };
    if session_id != u.session_id || user_id != u.user_id {
        return Err(Refusal::new(400, "ceremony_expired", "Try again."));
    }
    let cred = finish_registration(svc, req, &reg, user_id, &finish)?;
    ok_after_add(svc, req, &session_id, cred)
}

#[derive(Deserialize)]
struct Rename {
    nickname: String,
}

fn credential_rename(svc: &AuthService, req: &Req<'_>, id: &str) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    let b: Rename = body(req)?;
    let nickname = super::flows::clean_text(&b.nickname);
    if nickname.is_empty() {
        return Err(Refusal::new(
            400,
            "bad_request",
            "A nickname can't be empty.",
        ));
    }
    svc.mutate(|d| {
        let c = d
            .credentials
            .iter_mut()
            .find(|c| c.id == id && c.user_id == u.user_id)
            .ok_or_else(|| not_found("credential"))?;
        let detail = format!("{} → {nickname}", c.nickname);
        c.nickname.clone_from(&nickname);
        d.audit(audit_entry(svc, "credential_rename", &u.name, detail, req));
        Ok(())
    })?;
    Ok(ok_json(&json!({ "ok": true })))
}

/// Inside a store write: remove credential `id` of `owner`, unless it is
/// their last, and end the sessions it signed in — and, since they may
/// have been its, the owner's sessions from before sessions named their
/// credential — except `keep`, the session doing the removing. The
/// removed credential's description for the reply, and how many sessions
/// ended.
fn remove_credential(
    d: &mut AuthData,
    owner: Uuid,
    id: &str,
    keep: Option<&str>,
) -> Result<(Value, usize), Refusal> {
    let mine = d.credentials_of(owner).count();
    let c = d
        .credentials
        .iter()
        .find(|c| c.id == id && c.user_id == owner)
        .ok_or_else(|| not_found("credential"))?;
    if mine <= 1 {
        return Err(Refusal::new(
            409,
            "last_credential",
            "That is the only way in left on this account; add another first, or delete the user.",
        ));
    }
    if is_last_admin_key(d, owner, c) {
        return Err(Refusal::new(
            409,
            "last_key",
            "Admins sign in with a passkey or security key, and this is the last one on the account; add another first.",
        ));
    }
    let gone = credential_json(c, "", false, false);
    d.credentials.retain(|c| c.id != id);
    let before = d.sessions.len();
    d.sessions.retain(|s| {
        s.user_id != owner
            || keep == Some(s.id.as_str())
            || s.credential.as_deref().is_some_and(|c| c != id)
    });
    Ok((gone, before - d.sessions.len()))
}

/// Whether `c` is the last passkey or key of `owner`, an admin: codes
/// cannot sign an admin in, so that key stays.
fn is_last_admin_key(d: &AuthData, owner: Uuid, c: &super::store::Credential) -> bool {
    c.kind.webauthn_id().is_some()
        && d.user(owner).is_some_and(|u| u.role == Role::Admin)
        && d.keys_of(owner) <= 1
}

fn credential_remove(svc: &AuthService, req: &Req<'_>, id: &str) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    need_step_up(svc, &Principal::User(u.clone()))?;
    let (gone, ended) = svc.mutate(|d| {
        let (gone, ended) = remove_credential(d, u.user_id, id, Some(&u.session_id))?;
        let detail = format!(
            "{}: {}, {ended} session(s) it signed in ended",
            gone["kind"].as_str().unwrap_or_default(),
            gone["nickname"].as_str().unwrap_or_default()
        );
        d.audit(audit_entry(svc, "credential_remove", &u.name, detail, req));
        Ok((gone, ended))
    })?;
    Ok(ok_json(
        &json!({ "ok": true, "removed": gone, "sessions_revoked": ended }),
    ))
}

// ---- your sessions ----------------------------------------------------------------

fn sessions_list(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    // Read before `read`: both take the store's lock.
    let ids: Vec<String> = svc.read(|d| {
        d.sessions
            .iter()
            .filter(|s| s.user_id == u.user_id)
            .map(|s| s.id.clone())
            .collect()
    });
    let seen: std::collections::HashMap<String, u64> = ids
        .into_iter()
        .filter_map(|id| svc.last_seen(&id).map(|t| (id, t)))
        .collect();
    let list = svc.read(|d| {
        let mut v: Vec<Value> = d
            .sessions
            .iter()
            .filter(|s| s.user_id == u.user_id)
            .map(|s| {
                json!({
                    "id": s.id,
                    "created": s.created,
                    "last_seen": seen.get(&s.id).copied().unwrap_or(s.last_seen).max(s.last_seen),
                    "ip": s.ip,
                    "user_agent": s.user_agent,
                    "current": s.id == u.session_id,
                })
            })
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s["last_seen"].as_u64()));
        v
    });
    Ok(ok_json(&json!({ "sessions": list })))
}

fn session_revoke(svc: &AuthService, req: &Req<'_>, id: &str) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    svc.mutate(|d| {
        let before = d.sessions.len();
        d.sessions
            .retain(|s| !(s.id == id && s.user_id == u.user_id));
        if d.sessions.len() == before {
            return Err(not_found("session"));
        }
        d.audit(audit_entry(
            svc,
            "session_revoke",
            &u.name,
            String::new(),
            req,
        ));
        Ok(())
    })?;
    Ok(ok_json(&json!({ "ok": true })))
}

fn sessions_revoke_others(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    let n = svc.mutate(|d| {
        let before = d.sessions.len();
        d.sessions
            .retain(|s| s.user_id != u.user_id || s.id == u.session_id);
        let n = before - d.sessions.len();
        d.audit(audit_entry(
            svc,
            "session_revoke",
            &u.name,
            format!("{n} other session(s)"),
            req,
        ));
        Ok(n)
    })?;
    Ok(ok_json(&json!({ "ok": true, "revoked": n })))
}

/// A 15-minute invite to add a credential to your own account, on another
/// device or hostname.
fn reenrol(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let u = need_user(svc, req)?;
    need_step_up(svc, &Principal::User(u.clone()))?;
    let token = session::new_token();
    let now = svc.now();
    let inv = invite_record(
        session::hash_token(&token),
        InviteTarget::User { user_id: u.user_id },
        None,
        &u.name,
        u.user_id,
        now,
        REENROL_SECS,
    );
    let expires = inv.expires;
    svc.mutate(|d| {
        d.invites.push(inv);
        d.audit(audit_entry(
            svc,
            "invite_create",
            &u.name,
            "re-enrol myself".into(),
            req,
        ));
        Ok(())
    })?;
    let link = format!("{}/admin/invite#{token}", origin_of(req));
    Ok(ok_json(&json!({
        "qr": super::totp::qr_rows(&link),
        "link": link,
        "expires": expires,
    })))
}

// ---- other users (admin) -----------------------------------------------------------

fn parse_user(id: &str) -> Result<Uuid, Refusal> {
    Uuid::parse_str(id).map_err(|_| not_found("user"))
}

/// Admin with a fresh credential, and their audit name.
fn admin_stepped_up(svc: &AuthService, req: &Req<'_>) -> Result<String, Refusal> {
    let p = need_admin(svc, req)?;
    need_step_up(svc, &p)?;
    Ok(p.audit_name())
}

fn users_list(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    need_admin(svc, req)?;
    let list = svc.read(|d| {
        let admins = d.admin_count();
        let mut v: Vec<Value> = d
            .users
            .iter()
            .map(|u| {
                let count = |k: &str| d.credentials_of(u.id).filter(|c| c.kind.name() == k).count();
                let creds: Vec<Value> = d
                    .credentials_of(u.id)
                    .map(|c| json!({ "id": c.id, "kind": c.kind.name(), "nickname": c.nickname, "rp_id": c.kind.rp_id() }))
                    .collect();
                json!({
                    "id": u.id.to_string(),
                    "name": u.name,
                    "display_name": u.display_name,
                    "role": u.role.as_str(),
                    "created": u.created,
                    "last_sign_in": u.last_sign_in,
                    "passkeys": count("passkey"),
                    "security_keys": count("security_key"),
                    "apps": count("totp"),
                    "credentials": creds,
                    "sessions": d.sessions.iter().filter(|s| s.user_id == u.id).count(),
                    "last_admin": u.role == Role::Admin && admins == 1,
                    "codes_locked_until": Some(super::totp::any_locked_until(&u.totp)).filter(|&t| t > svc.now()),
                })
            })
            .collect();
        v.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        v
    });
    Ok(ok_json(&json!({ "users": list })))
}

fn last_admin() -> Refusal {
    Refusal::new(
        409,
        "last_admin",
        "This is the only admin; make someone else an admin first.",
    )
}

#[derive(Deserialize)]
struct RoleChange {
    role: String,
}

fn user_role(svc: &AuthService, req: &Req<'_>, id: &str) -> Result<HttpResponse, Refusal> {
    let who = admin_stepped_up(svc, req)?;
    let id = parse_user(id)?;
    let b: RoleChange = body(req)?;
    let role = Role::parse(&b.role)
        .ok_or_else(|| Refusal::new(400, "bad_request", "The role is admin or viewer."))?;
    svc.mutate(|d| {
        let admins = d.admin_count();
        let keys = d.keys_of(id);
        let u = d.user_mut(id).ok_or_else(|| not_found("user"))?;
        if u.role == Role::Admin && role == Role::Viewer && admins <= 1 {
            return Err(last_admin());
        }
        if u.role == Role::Viewer && role == Role::Admin && keys == 0 {
            return Err(Refusal::new(
                409,
                "key_required",
                format!(
                    "{} has no passkey or security key, and admins must sign in with one. Send them a re-enrol link to add one, then try again.",
                    u.name
                ),
            ));
        }
        let demoted = u.role == Role::Admin && role == Role::Viewer;
        let detail = format!("{}: {} → {}", u.name, u.role.as_str(), role.as_str());
        u.role = role;
        // A listener cannot invite, so what they invited as an admin goes.
        let withdrawn = if demoted {
            d.withdraw_invites_by(id, true)
        } else {
            0
        };
        let detail = if withdrawn > 0 {
            format!("{detail}, {withdrawn} invite(s) they made withdrawn")
        } else {
            detail
        };
        d.audit(audit_entry(svc, "role_change", &who, detail, req));
        Ok(())
    })?;
    Ok(ok_json(&json!({ "ok": true })))
}

fn user_delete(svc: &AuthService, req: &Req<'_>, id: &str) -> Result<HttpResponse, Refusal> {
    let who = admin_stepped_up(svc, req)?;
    let id = parse_user(id)?;
    svc.mutate(|d| {
        let u = d.user(id).ok_or_else(|| not_found("user"))?;
        if u.role == Role::Admin && d.admin_count() <= 1 {
            return Err(last_admin());
        }
        let name = u.name.clone();
        remove_user(d, id);
        d.audit(audit_entry(svc, "user_delete", &who, name, req));
        Ok(())
    })?;
    Ok(ok_json(&json!({ "ok": true })))
}

fn user_sign_out(svc: &AuthService, req: &Req<'_>, id: &str) -> Result<HttpResponse, Refusal> {
    let p = need_admin(svc, req)?;
    let id = parse_user(id)?;
    let (n, invites) = svc.mutate(|d| {
        let name = d.user(id).ok_or_else(|| not_found("user"))?.name.clone();
        let before = d.sessions.len();
        d.sessions.retain(|s| s.user_id != id);
        let n = before - d.sessions.len();
        // Whoever held one of those sessions may have minted links with it.
        let invites = d.withdraw_invites_by(id, false);
        d.audit(audit_entry(
            svc,
            "session_revoke",
            &p.audit_name(),
            format!("{name}: everywhere ({n}), {invites} pending invite(s) withdrawn"),
            req,
        ));
        Ok((n, invites))
    })?;
    Ok(ok_json(
        &json!({ "ok": true, "revoked": n, "invites_revoked": invites }),
    ))
}

fn user_credential_remove(
    svc: &AuthService,
    req: &Req<'_>,
    id: &str,
    cid: &str,
) -> Result<HttpResponse, Refusal> {
    let p = need_admin(svc, req)?;
    need_step_up(svc, &p)?;
    let who = p.audit_name();
    let id = parse_user(id)?;
    // An admin removing one of their own keeps the session doing it.
    let keep = match &p {
        Principal::User(u) if u.user_id == id => Some(u.session_id.clone()),
        _ => None,
    };
    let ended = svc.mutate(|d| {
        let name = d.user(id).ok_or_else(|| not_found("user"))?.name.clone();
        let (gone, ended) = remove_credential(d, id, cid, keep.as_deref())?;
        let detail = format!(
            "{name}'s {}: {}, {ended} session(s) it signed in ended",
            gone["kind"].as_str().unwrap_or_default(),
            gone["nickname"].as_str().unwrap_or_default()
        );
        d.audit(audit_entry(svc, "credential_remove", &who, detail, req));
        Ok(ended)
    })?;
    Ok(ok_json(&json!({ "ok": true, "sessions_revoked": ended })))
}

fn audit(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    need_admin(svc, req)?;
    let filter = req
        .query
        .split('&')
        .find_map(|kv| kv.strip_prefix("user="))
        .filter(|u| !u.is_empty())
        .map(str::to_ascii_lowercase);
    let entries = svc.read(|d| {
        d.audit
            .iter()
            .rev()
            .filter(|e| {
                filter.as_deref().is_none_or(|f| {
                    e.user.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(f))
                        || e.detail.to_ascii_lowercase().contains(f)
                })
            })
            .map(|e| json!({ "at": e.at, "event": e.event, "user": e.user, "detail": e.detail, "ip": e.ip, "times": e.times.max(1), "since": e.since }))
            .collect::<Vec<_>>()
    });
    Ok(ok_json(&json!({ "entries": entries })))
}
