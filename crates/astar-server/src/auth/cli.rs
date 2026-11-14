// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `astar-server admin …`: bootstrap and recovery from the console.
//!
//! ```text
//! astar-server admin invite --role admin|viewer [--name W1ABC] [--hours 1]
//! astar-server admin invite --user W1ABC [--hours 1]
//! astar-server admin users
//! astar-server admin revoke-sessions --user W1ABC
//! ```
//!
//! The CLI makes the invite token itself and prints the link with
//! `println!`, never through `tracing`. When the server is running it sends
//! only the token's SHA-256 and the invite's fields to `POST
//! /auth/cli/invite` on the loopback control port, so the token never
//! crosses a socket and the server's in-memory store stays the one that
//! counts. When it is not running, the CLI writes `auth.json` itself under
//! the store lock. Console access is already root-equivalent on a node, so
//! this is no new door: it is the way in when every credential is lost.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::NodeFileConfig;
use crate::http::HttpResponse;

use super::flows::{body, default_lifetime, invite_record, valid_name};
use super::ok_json;
use super::service::{AuthService, Refusal, Req, Surface};
use super::session;
use super::store::{self, AuditEntry, InviteTarget, Loaded, Role};

/// Where `node.toml` is looked for when `--config` is not given: the
/// container's path, then the systemd unit's.
pub const DEFAULT_CONFIGS: &[&str] = &["/etc/iaxnode/node.toml", "/etc/astar/node.toml"];

/// Who an invite from the console is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliTarget {
    Role(Role),
    /// Re-enrol the named user.
    User(String),
}

/// A parsed `admin` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Invite {
        target: CliTarget,
        name: Option<String>,
        /// Seconds.
        lifetime: Option<u64>,
    },
    Users,
    RevokeSessions {
        user: String,
    },
}

/// What the CLI needs from `node.toml`.
pub struct Node {
    pub state_dir: PathBuf,
    pub control_bind: String,
    /// `https://<first public hostname>[:port]`, when there is a public bind.
    pub public_origin: Option<String>,
}

/// Parse `admin <command> [flags]`; `--config` is returned beside it.
///
/// # Errors
/// A sentence for the operator.
pub fn parse(args: &[String]) -> Result<(Command, Option<PathBuf>), String> {
    let mut it = args.iter();
    let sub = it
        .next()
        .ok_or("admin: use invite, users or revoke-sessions")?;
    let mut role = None;
    let mut user = None;
    let mut name = None;
    let mut hours: Option<f64> = None;
    let mut config = None;
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("admin: {a} needs a value"))
        };
        match a.as_str() {
            "--role" => role = Some(val()?),
            "--user" => user = Some(val()?),
            "--name" => name = Some(val()?),
            "--hours" => {
                hours = Some(
                    val()?
                        .parse()
                        .map_err(|_| "admin: --hours is a number".to_string())?,
                );
            }
            "--config" | "-c" => config = Some(PathBuf::from(val()?)),
            other => return Err(format!("admin: unknown option {other:?}")),
        }
    }
    let lifetime = match hours {
        None => None,
        Some(h) if h.is_finite() && (h * 3600.0) >= 300.0 && (h * 3600.0) <= 604_800.0 =>
        {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "checked to lie within 300..=604800 above"
            )]
            Some((h * 3600.0).round() as u64)
        }
        Some(_) => {
            return Err("admin: --hours is between 0.09 (5 minutes) and 168 (7 days)".into());
        }
    };
    let cmd = match sub.as_str() {
        "invite" => {
            let target = match (role.as_deref(), user) {
                (None, Some(u)) => CliTarget::User(u),
                (Some(r), None) => {
                    CliTarget::Role(Role::parse(r).ok_or("admin: --role is admin or viewer")?)
                }
                (Some(_), Some(_)) => return Err("admin: give --role or --user, not both".into()),
                (None, None) => {
                    return Err(
                        "admin invite: --role admin|viewer, or --user NAME to re-enrol".into(),
                    );
                }
            };
            if let Some(n) = &name
                && !valid_name(n)
            {
                return Err(
                    "admin: --name is 1 to 32 letters, digits, '-', '_', '.' or '/'".into(),
                );
            }
            Command::Invite {
                target,
                name,
                lifetime,
            }
        }
        "users" => Command::Users,
        "revoke-sessions" => Command::RevokeSessions {
            user: user.ok_or("admin revoke-sessions: --user NAME")?,
        },
        other => return Err(format!("admin: unknown command {other:?}")),
    };
    Ok((cmd, config))
}

/// Read what the CLI needs from `node.toml`.
///
/// # Errors
/// The file cannot be read or does not load.
pub fn node(config: &Path) -> Result<Node, String> {
    let text = std::fs::read_to_string(config)
        .map_err(|e| format!("cannot read {}: {e}", config.display()))?;
    let cfg =
        NodeFileConfig::from_toml_str(&text).map_err(|e| format!("{}: {e}", config.display()))?;
    let public_origin = cfg.web.as_ref().and_then(|w| {
        let host = w.hostnames().first()?;
        let port = w.public_bind.as_deref()?.parse::<SocketAddr>().ok()?.port();
        Some(if port == 443 {
            format!("https://{host}")
        } else {
            format!("https://{host}:{port}")
        })
    });
    Ok(Node {
        state_dir: crate::config::state_dir(config),
        control_bind: cfg.control.bind.clone(),
        public_origin,
    })
}

/// Run an `admin` command and say what happened.
pub fn run(args: &[String]) -> ExitCode {
    let (cmd, config) = match parse(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("astar-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(config) = config.or_else(|| {
        DEFAULT_CONFIGS
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
    }) else {
        eprintln!("astar-server: admin: no node.toml found; pass --config <path>");
        return ExitCode::FAILURE;
    };
    let node = match node(&config) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("astar-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    let out = match cmd {
        Command::Invite {
            target,
            name,
            lifetime,
        } => invite(&node, &target, name, lifetime).map(|(link, note)| {
            // stdout, once: the link IS the credential until it is used.
            println!("{link}");
            if let Some(n) = note {
                println!("{n}");
            }
        }),
        Command::Users => users(&node).map(|lines| {
            for l in lines {
                println!("{l}");
            }
        }),
        Command::RevokeSessions { user } => {
            revoke_sessions(&node, &user).map(|n| println!("signed {user} out of {n} session(s)"))
        }
    };
    match out {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("astar-server: admin: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Make an invite: through the running server when there is one, else in
/// the file. The link, and a hint for reaching it when there is no public
/// page.
///
/// # Errors
/// A sentence for the operator.
pub fn invite(
    node: &Node,
    target: &CliTarget,
    name: Option<String>,
    lifetime: Option<u64>,
) -> Result<(String, Option<String>), String> {
    let token = session::new_token();
    let hash = session::hash_token(&token);
    let request = json!({
        "hash": hash,
        "role": match target { CliTarget::Role(r) => Some(r.as_str()), CliTarget::User(_) => None },
        "user": match target { CliTarget::User(u) => Some(u.as_str()), CliTarget::Role(_) => None },
        "name": name,
        "lifetime": lifetime,
    });
    match post_local(&node.control_bind, "/auth/cli/invite", &request) {
        Ok((200, _)) => {}
        Ok((_, v)) => {
            return Err(v["message"]
                .as_str()
                .unwrap_or("the node refused the invite")
                .to_string());
        }
        Err(Offline) => invite_offline(&node.state_dir, &hash, target, name, lifetime)?,
    }
    let port = node
        .control_bind
        .parse::<SocketAddr>()
        .map_or(8730, |a| a.port());
    Ok(match &node.public_origin {
        Some(origin) => (format!("{origin}/admin/invite#{token}"), None),
        None => (
            format!("http://localhost:{port}/admin/invite#{token}"),
            Some(format!(
                "This node has no public page: open the link through a tunnel, from your own machine:\n  ssh -N -L {port}:127.0.0.1:{port} <you>@<this node>"
            )),
        ),
    })
}

/// The server is not answering on the control port.
struct Offline;

/// POST JSON to the control port on loopback; `Err(Offline)` when nothing
/// is listening.
fn post_local(bind: &str, path: &str, body: &Value) -> Result<(u16, Value), Offline> {
    let addr: SocketAddr = bind.parse().map_err(|_| Offline)?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|_| Offline)?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let payload = body.to_string();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    s.write_all(head.as_bytes()).map_err(|_| Offline)?;
    s.write_all(payload.as_bytes()).map_err(|_| Offline)?;
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let v = raw
        .split_once("\r\n\r\n")
        .and_then(|(_, b)| serde_json::from_str(b).ok())
        .unwrap_or(Value::Null);
    Ok((status, v))
}

/// The store for a CLI that writes it: refused when sign-in would be.
fn load_for_cli(dir: &Path) -> Result<store::AuthData, String> {
    match store::load(dir) {
        Loaded::Ok(d) => Ok(d),
        Loaded::Disabled(why) => Err(why),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Write an invite into `auth.json` under the lock (the server is down).
///
/// # Errors
/// A store that cannot be trusted or written, or an unknown user.
pub fn invite_offline(
    dir: &Path,
    hash: &str,
    target: &CliTarget,
    name: Option<String>,
    lifetime: Option<u64>,
) -> Result<(), String> {
    let _held = store::lock(dir)?;
    let mut d = load_for_cli(dir)?;
    let target = resolve(&d, target).map_err(|r| r.message)?;
    let now = now();
    let lifetime = lifetime.unwrap_or_else(|| default_lifetime(&target));
    d.invites.push(invite_record(
        hash.to_string(),
        target,
        name,
        "console",
        store::CONSOLE_CREATOR,
        now,
        lifetime,
    ));
    d.audit(AuditEntry {
        at: now,
        event: "invite_create".into(),
        user: Some("console".into()),
        detail: "from the command line".into(),
        ip: String::new(),
        times: 0,
        since: None,
    });
    store::write_locked(dir, &d)
}

fn resolve(d: &store::AuthData, target: &CliTarget) -> Result<InviteTarget, Refusal> {
    Ok(match target {
        CliTarget::Role(role) => InviteTarget::NewUser { role: *role },
        CliTarget::User(name) => InviteTarget::User {
            user_id: d
                .user_by_name(name)
                .ok_or_else(|| Refusal::new(404, "unknown_user", format!("no user named {name}")))?
                .id,
        },
    })
}

/// `admin users`: from the file, which every change is written to at once.
///
/// # Errors
/// A store that cannot be read.
pub fn users(node: &Node) -> Result<Vec<String>, String> {
    let d = load_for_cli(&node.state_dir)?;
    if d.users.is_empty() {
        return Ok(vec![
            "no users yet: astar-server admin invite --role admin".to_string(),
        ]);
    }
    Ok(d.users
        .iter()
        .map(|u| {
            let count = |k: &str| {
                d.credentials_of(u.id)
                    .filter(|c| c.kind.name() == k)
                    .count()
            };
            format!(
                "{:<12} {:<6} passkeys {}  keys {}  apps {}  sessions {}",
                u.name,
                u.role.as_str(),
                count("passkey"),
                count("security_key"),
                count("totp"),
                d.sessions.iter().filter(|s| s.user_id == u.id).count()
            )
        })
        .collect())
}

/// `admin revoke-sessions --user N`: through the server, or the file.
///
/// # Errors
/// An unknown user or an unwritable store.
pub fn revoke_sessions(node: &Node, user: &str) -> Result<usize, String> {
    match post_local(
        &node.control_bind,
        "/auth/cli/revoke-sessions",
        &json!({ "user": user }),
    ) {
        Ok((200, v)) => Ok(usize::try_from(v["revoked"].as_u64().unwrap_or(0)).unwrap_or(0)),
        Ok((_, v)) => Err(v["message"]
            .as_str()
            .unwrap_or("the node refused")
            .to_string()),
        Err(Offline) => {
            let _held = store::lock(&node.state_dir)?;
            let mut d = load_for_cli(&node.state_dir)?;
            let id = d
                .user_by_name(user)
                .ok_or_else(|| format!("no user named {user}"))?
                .id;
            let before = d.sessions.len();
            d.sessions.retain(|s| s.user_id != id);
            let n = before - d.sessions.len();
            d.withdraw_invites_by(id, false);
            store::write_locked(&node.state_dir, &d)?;
            Ok(n)
        }
    }
}

// ---- the server's side ----------------------------------------------------------

/// The CLI's way in: a loopback peer on the control port, with no proxy
/// header, whatever `loopback_login` says (it is the recovery path). Absent
/// everywhere else, as a 404.
fn from_the_console(req: &Req<'_>) -> Result<(), Refusal> {
    if req.surface == Surface::Control
        && crate::access::is_trusted_loopback_peer(req.peer, req.forwarded)
    {
        Ok(())
    } else {
        Err(Refusal::new(404, "not_found", "not found"))
    }
}

#[derive(Deserialize)]
struct CliInvite {
    hash: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    lifetime: Option<u64>,
}

/// `POST /auth/cli/invite` and `POST /auth/cli/revoke-sessions`.
///
/// # Errors
/// 404 off the console; the refusal otherwise.
pub fn server_route(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    from_the_console(req)?;
    match req.path {
        "/auth/cli/invite" => cli_invite(svc, req),
        "/auth/cli/revoke-sessions" => cli_revoke(svc, req),
        _ => Err(Refusal::new(404, "not_found", "not found")),
    }
}

fn cli_invite(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let b: CliInvite = body(req)?;
    let bad = |m: &str| Refusal::new(400, "bad_request", m.to_string());
    if b.hash.len() != 64 || !b.hash.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad("hash is a hex SHA-256"));
    }
    let target = match (b.role.as_deref(), b.user) {
        (Some(r), None) => {
            CliTarget::Role(Role::parse(r).ok_or_else(|| bad("role is admin or viewer"))?)
        }
        (None, Some(u)) => CliTarget::User(u),
        _ => return Err(bad("role or user")),
    };
    if let Some(n) = &b.name
        && !valid_name(n)
    {
        return Err(bad("bad name"));
    }
    let now = svc.now();
    let ip = req.ip();
    let expires = svc.mutate(|d| {
        let target = resolve(d, &target)?;
        let lifetime = b.lifetime.unwrap_or_else(|| default_lifetime(&target));
        let inv = invite_record(
            b.hash.to_ascii_lowercase(),
            target,
            b.name.clone(),
            "console",
            store::CONSOLE_CREATOR,
            now,
            lifetime,
        );
        let expires = inv.expires;
        d.invites.push(inv);
        d.audit(AuditEntry {
            at: now,
            event: "invite_create".into(),
            user: Some("console".into()),
            detail: "from the command line".into(),
            ip,
            times: 0,
            since: None,
        });
        Ok(expires)
    })?;
    Ok(ok_json(&json!({ "ok": true, "expires": expires })))
}

#[derive(Deserialize)]
struct CliRevoke {
    user: String,
}

fn cli_revoke(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let b: CliRevoke = body(req)?;
    let now = svc.now();
    let ip = req.ip();
    let n = svc.mutate(|d| {
        let id = d
            .user_by_name(&b.user)
            .ok_or_else(|| Refusal::new(404, "unknown_user", format!("no user named {}", b.user)))?
            .id;
        let before = d.sessions.len();
        d.sessions.retain(|s| s.user_id != id);
        let n = before - d.sessions.len();
        let invites = d.withdraw_invites_by(id, false);
        d.audit(AuditEntry {
            at: now,
            event: "session_revoke".into(),
            user: Some("console".into()),
            detail: format!(
                "{}: everywhere ({n}), {invites} pending invite(s) withdrawn",
                b.user
            ),
            ip,
            times: 0,
            since: None,
        });
        Ok(n)
    })?;
    Ok(ok_json(&json!({ "ok": true, "revoked": n })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn parsing() {
        let (c, cfg) = parse(&args(
            "invite --role admin --name W1ABC --hours 2 --config /x/node.toml",
        ))
        .unwrap();
        assert_eq!(
            c,
            Command::Invite {
                target: CliTarget::Role(Role::Admin),
                name: Some("W1ABC".into()),
                lifetime: Some(7200)
            }
        );
        assert_eq!(cfg, Some(PathBuf::from("/x/node.toml")));
        assert!(matches!(
            parse(&args("invite --user W1ABC")).unwrap().0,
            Command::Invite {
                target: CliTarget::User(_),
                ..
            }
        ));
        assert_eq!(parse(&args("users")).unwrap().0, Command::Users);
        for bad in [
            "",
            "invite",
            "invite --role root",
            "invite --role admin --user x",
            "invite --role admin --hours 500",
            "nope",
            "revoke-sessions",
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad}");
        }
    }

    fn write_node(dir: &Path, bind: &str, web: &str) -> PathBuf {
        let p = dir.join("node.toml");
        std::fs::write(
            &p,
            format!(
                "[listener]\nbind = \"127.0.0.1:0\"\nanswer = \"auto\"\nmax_calls = 1\nauth = \"optional\"\n[control]\nbind = \"{bind}\"\n[secrets]\nsource = \"env\"\n{web}"
            ),
        )
        .unwrap();
        p
    }

    #[test]
    fn an_offline_invite_is_written_under_the_lock_and_hashed() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing listens on this port: bind and drop.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let cfg = write_node(
            dir.path(),
            &format!("127.0.0.1:{port}"),
            "[web]\npublic_bind = \"0.0.0.0:443\"\npublic_hostnames = [\"node.example.test\"]\n",
        );
        let node = node(&cfg).unwrap();
        let (link, hint) = invite(
            &node,
            &CliTarget::Role(Role::Admin),
            Some("W1ABC".into()),
            None,
        )
        .unwrap();
        assert!(
            link.starts_with("https://node.example.test/admin/invite#"),
            "{link}"
        );
        assert!(hint.is_none());
        let token = link.split_once('#').unwrap().1;
        let Loaded::Ok(d) = store::load(&node.state_dir) else {
            panic!()
        };
        assert_eq!(d.invites.len(), 1);
        assert_eq!(d.invites[0].hash, session::hash_token(token));
        assert_eq!(
            d.invites[0].expires - d.invites[0].created,
            3600,
            "admin: an hour"
        );
        assert!(
            !std::fs::read_to_string(store::store_path(&node.state_dir))
                .unwrap()
                .contains(token)
        );
        assert_eq!(
            users(&node).unwrap(),
            ["no users yet: astar-server admin invite --role admin"]
        );
    }

    #[test]
    fn without_a_public_page_the_link_is_for_a_tunnel() {
        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let cfg = write_node(dir.path(), &format!("127.0.0.1:{port}"), "");
        let (link, note) = invite(
            &node(&cfg).unwrap(),
            &CliTarget::Role(Role::Viewer),
            None,
            None,
        )
        .unwrap();
        assert!(
            link.starts_with(&format!("http://localhost:{port}/admin/invite#")),
            "{link}"
        );
        assert!(
            note.unwrap()
                .contains(&format!("ssh -N -L {port}:127.0.0.1:{port}"))
        );
    }

    #[test]
    fn a_running_server_gets_only_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let svc = Arc::new(AuthService::open(super::super::AuthSettings::defaults(
            &state,
        )));
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let ctrl = Arc::new(crate::controller::NodeController::new(
            station,
            crate::secrets::SecretProvider::new(),
        ));
        ctrl.set_auth(Arc::clone(&svc));
        let server = crate::server::bind_control(&ctrl, "127.0.0.1:0").unwrap();
        let bind = server.local_addr().to_string();
        let node = Node {
            state_dir: state.clone(),
            control_bind: bind,
            public_origin: None,
        };
        let (link, _) = invite(&node, &CliTarget::Role(Role::Admin), None, None).unwrap();
        let token = link.split_once('#').unwrap().1.to_string();
        let hashes: Vec<String> = svc.read(|d| d.invites.iter().map(|i| i.hash.clone()).collect());
        assert_eq!(
            hashes,
            [session::hash_token(&token)],
            "the server holds it, hashed"
        );
        // Revoking sessions also goes through the server.
        svc.test_sign_in("W1ABC", Role::Admin);
        assert_eq!(revoke_sessions(&node, "W1ABC").unwrap(), 1);
        assert!(revoke_sessions(&node, "NOBODY").is_err());
        server.stop();
    }

    #[test]
    fn the_cli_route_is_only_for_a_loopback_peer_on_the_control_port() {
        let dir = tempfile::tempdir().unwrap();
        let svc = AuthService::open(super::super::AuthSettings::defaults(dir.path()));
        let body = json!({ "hash": "a".repeat(64), "role": "admin" }).to_string();
        let req = |surface: Surface, peer: &str, forwarded: bool| {
            let r = Req {
                method: "POST",
                path: "/auth/cli/invite",
                query: "",
                body: body.as_bytes(),
                host: Some("localhost"),
                origin: None,
                content_type: Some("application/json"),
                sec_fetch_site: None,
                cookie: None,
                user_agent: None,
                peer: Some(peer.parse().unwrap()),
                forwarded,
                surface,
                tls: false,
            };
            super::super::handle(&svc, &r).unwrap().status
        };
        assert_eq!(req(Surface::Public, "127.0.0.1:1", false), 404);
        assert_eq!(req(Surface::Control, "192.168.1.2:1", false), 404);
        assert_eq!(req(Surface::Control, "127.0.0.1:1", true), 404);
        assert_eq!(req(Surface::Control, "127.0.0.1:1", false), 200);
        // And it still works with loopback_login on: it is the recovery path.
        let strict = AuthService::open({
            let mut s = super::super::AuthSettings::defaults(dir.path());
            s.loopback_login = true;
            s
        });
        let r = Req {
            method: "POST",
            path: "/auth/cli/invite",
            query: "",
            body: body.as_bytes(),
            host: Some("localhost"),
            origin: None,
            content_type: Some("application/json"),
            sec_fetch_site: None,
            cookie: None,
            user_agent: None,
            peer: Some("127.0.0.1:1".parse().unwrap()),
            forwarded: false,
            surface: Surface::Control,
            tls: false,
        };
        assert_eq!(super::super::handle(&strict, &r).unwrap().status, 200);
    }
}
