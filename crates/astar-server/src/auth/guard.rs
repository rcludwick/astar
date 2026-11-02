// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The hostname guard rail on settings saves.
//!
//! Every passkey and security key is bound to an RP ID. Saving
//! `web.public_hostnames` or `web.rp_id` so that no remaining hostname uses
//! an RP ID a credential was made for strands that credential. The save
//! then needs an acknowledgement that names how many credentials, and
//! whose, would stop working; and when it would leave no admin able to sign
//! in on the public page at all (no key that still works, and no codes,
//! which sign in only an admin with no key at all) it is
//! refused, with the console command that recovers from it. Both are the
//! settings page's per-field 400, which it already knows how to show.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::http::HttpResponse;

use super::service::AuthService;
use super::store::{CredentialKind, Role};
use super::webauthn::rp_id_for;

/// The acknowledgement a stranding save carries.
pub const ACK_STRAND: &str = "web.hostnames.strand";

/// Check a `POST /settings` body. `None` lets it through; `Some` is the
/// answer instead.
#[must_use]
pub fn check_settings_save(svc: &AuthService, body: &[u8]) -> Option<HttpResponse> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let patch = v.get("patch")?.as_object()?;
    let hosts_patch = patch.get("web.public_hostnames");
    let rp_patch = patch.get("web.rp_id");
    if hosts_patch.is_none() && rp_patch.is_none() {
        return None;
    }
    // The hostnames after the save: the patch's list (with `{node}`
    // expanded as the loader would), or today's when only rp_id changes. A
    // patch that removes the list falls back to the DNS default, which this
    // cannot see; it is left to the loader.
    let hosts: Vec<String> = match hosts_patch {
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .map(|h| {
                let h = svc.settings.node.as_deref().map_or_else(
                    || h.to_string(),
                    |n| h.replace(crate::config::NODE_TOKEN, n),
                );
                crate::config::normalize_hostname(&h)
            })
            .collect(),
        Some(_) => return None,
        None => svc.settings.public_hostnames.clone(),
    };
    let rp = match rp_patch {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            Some(crate::config::normalize_hostname(s))
        }
        Some(Value::String(_) | Value::Null) => None,
        Some(_) => return None,
        None => svc.settings.rp_id.clone(),
    };
    let live: BTreeSet<String> = hosts.iter().map(|h| rp_id_for(rp.as_deref(), h)).collect();

    let (stranded, users, admin_left, any_admin) = svc.read(|d| {
        let mut stranded = 0usize;
        let mut users = BTreeSet::new();
        for c in &d.credentials {
            if let Some(r) = c.kind.rp_id()
                && !live.contains(r)
            {
                stranded += 1;
                if let Some(u) = d.user(c.user_id) {
                    users.insert(u.name.clone());
                }
            }
        }
        let admin_left = d.users.iter().filter(|u| u.role == Role::Admin).any(|u| {
            d.credentials_of(u.id).any(|c| match &c.kind {
                // Codes sign an admin in only while they have no key at all.
                CredentialKind::Totp { .. } => d.codes_sign_in(u.id),
                k => k.rp_id().is_some_and(|r| live.contains(r)),
            })
        });
        (stranded, users, admin_left, d.admin_count() > 0)
    });
    if stranded == 0 {
        return None;
    }
    let who = users.into_iter().collect::<Vec<_>>().join(", ");
    if any_admin && !admin_left {
        // 400 with a field and no acknowledgement: the settings page pins
        // it to the field and offers no way past it. (A 409 there means the
        // file changed underneath the page.)
        return Some(HttpResponse::json(
            400,
            &json!({
                "error": format!(
                    "web.public_hostnames: no admin could sign in on the public page after this ({stranded} passkey/key credential(s) of {who} would stop working, and authenticator-app codes don't sign admins in). Keep a hostname they use, or recover afterwards from the node's console: astar-server admin invite --user <name>"
                ),
                "field": "web.public_hostnames",
            }),
        ));
    }
    let acked = v
        .get("acknowledge")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|k| k.as_str() == Some(ACK_STRAND)));
    if acked {
        return None;
    }
    Some(HttpResponse::json(
        400,
        &json!({
            "error": format!(
                "web.public_hostnames: {stranded} passkey/key credential(s) of {who} would stop working, because no hostname would use the name they were made on. Before saving, each of them can use Account → Set up another device or hostname to add a passkey on the new name; a listener can also sign in there with an authenticator app."
            ),
            "field": "web.public_hostnames",
            "acknowledge": ACK_STRAND,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::service::tests::Fixture;
    use crate::auth::session;
    use crate::auth::store::{Credential, CredentialKind};

    /// A user whose only credential is an authenticator app, which works on
    /// any hostname.
    fn add_totp(f: &Fixture, name: &str, role: Role) {
        f.svc.test_sign_in(name, role);
        let uid = f.svc.read(|d| d.user_by_name(name).unwrap().id);
        f.svc
            .mutate(|d| {
                d.credentials.push(Credential {
                    id: session::new_id(),
                    user_id: uid,
                    nickname: "app".into(),
                    created: 0,
                    last_used: None,
                    kind: CredentialKind::Totp {
                        secret: "JBSWY3DPEHPK3PXP".into(),
                        last_step: 0,
                    },
                });
                Ok(())
            })
            .unwrap();
    }

    /// Register a real passkey for `name` on the fixture's host.
    fn add_passkey(f: &Fixture, role: &str, name: &str) {
        use crate::auth::tests_e2e::{Browser, invite};
        use webauthn_authenticator_rs::WebauthnAuthenticator;
        use webauthn_authenticator_rs::softpasskey::SoftPasskey;
        let token = invite(&f.svc, &json!({ "role": role }));
        let mut b = Browser::new(&f.svc);
        let mut pk = WebauthnAuthenticator::new(SoftPasskey::new(true));
        let (_, v) = b.call(
            "POST",
            "/auth/invite/begin",
            &json!({ "token": token, "name": name, "kind": "passkey" }),
        );
        let ccr = serde_json::from_value(v["options"].clone()).unwrap();
        let reg = pk.do_registration(b.origin(), ccr).unwrap();
        let (st, _) = b.call("POST", "/auth/invite/finish", &json!({ "credential": reg }));
        assert_eq!(st, 200);
    }

    fn save(hosts: &[&str], ack: bool) -> Vec<u8> {
        json!({
            "etag": "x",
            "patch": { "web.public_hostnames": hosts },
            "acknowledge": if ack { vec![ACK_STRAND] } else { vec![] },
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn a_save_that_strands_the_only_admin_is_refused() {
        let f = Fixture::new();
        add_passkey(&f, "admin", "W1ABC");
        // BOSS (the invite's maker) has no credential; W1ABC is the admin
        // with a way in, and it is a passkey on node.example.test.
        let r = check_settings_save(&f.svc, &save(&["new.example.org"], true)).unwrap();
        assert_eq!(r.status, 400, "refused even when acknowledged");
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(
            v.get("acknowledge").is_none(),
            "no acknowledgement unlocks it"
        );
        let r = check_settings_save(&f.svc, &save(&["new.example.org"], true)).unwrap();
        let msg = String::from_utf8(r.body).unwrap();
        assert!(
            msg.contains("W1ABC") && msg.contains("astar-server admin invite"),
            "{msg}"
        );
    }

    #[test]
    fn a_save_that_strands_some_credentials_needs_an_acknowledgement() {
        let f = Fixture::new();
        add_passkey(&f, "viewer", "V1EW");
        add_totp(&f, "A1DM", Role::Admin);
        let r = check_settings_save(&f.svc, &save(&["new.example.org"], false)).unwrap();
        assert_eq!(r.status, 400);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["acknowledge"], ACK_STRAND);
        assert!(v["error"].as_str().unwrap().contains("V1EW"));
        assert!(check_settings_save(&f.svc, &save(&["new.example.org"], true)).is_none());
    }

    #[test]
    fn an_admins_app_does_not_count_once_they_have_a_key() {
        let f = Fixture::new();
        add_passkey(&f, "admin", "W1ABC");
        let uid = f.svc.read(|d| d.user_by_name("W1ABC").unwrap().id);
        f.svc
            .mutate(|d| {
                d.credentials.push(Credential {
                    id: session::new_id(),
                    user_id: uid,
                    nickname: "app".into(),
                    created: 0,
                    last_used: None,
                    kind: CredentialKind::Totp {
                        secret: "JBSWY3DPEHPK3PXP".into(),
                        last_step: 0,
                    },
                });
                Ok(())
            })
            .unwrap();
        // Their codes only step up; with the passkey stranded, they could
        // not sign in at all.
        let r = check_settings_save(&f.svc, &save(&["new.example.org"], true)).unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(r.status == 400 && v.get("acknowledge").is_none(), "{v}");
    }

    #[test]
    fn keeping_the_hostname_or_a_parent_rp_id_strands_nothing() {
        let f = Fixture::new();
        add_passkey(&f, "admin", "W1ABC");
        assert!(
            check_settings_save(
                &f.svc,
                &save(&["node.example.test", "new.example.org"], false)
            )
            .is_none()
        );
        let unrelated =
            json!({ "etag": "x", "patch": { "bridge.mode": "conference" } }).to_string();
        assert!(check_settings_save(&f.svc, unrelated.as_bytes()).is_none());
        // Moving to a parent rp_id strands credentials made on the exact name.
        let to_parent =
            json!({ "etag": "x", "patch": { "web.rp_id": "example.test" } }).to_string();
        let r = check_settings_save(&f.svc, to_parent.as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(v.get("acknowledge").is_none() && r.status == 400, "{v}");
    }
}
